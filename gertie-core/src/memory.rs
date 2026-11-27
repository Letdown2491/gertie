//! Long-term memory: markdown tiers on disk, a derived SQLite sidecar.
//!
//! The files are the record; the database is an index. Every read of a
//! memory fact starts at a hash check: a file the harness has not seen
//! change is never re-parsed, a hand-edited file is picked up whole on
//! the next sync. Nothing here deletes: superseding and archiving set
//! `valid_until` and mark the file line, so "what did we believe when
//! we wrote this code" stays a query and a git diff stays honest.
//!
//! Entry grammar (one line per entry, in a topic file):
//!
//! `- [p12] helpful=2 harmful=0 the text of the entry [source: 388; added: 2026-07-11]`
//!
//! The id prefix carries the tier (`p` project, `g` global), so ids stay
//! unique across the two roots without coordination. Superseded and
//! archived lines end in `[valid until: DATE]` or `[archived: DATE]`;
//! both parse back into `valid_until` and both keep history queryable.
//! A supersede that names its replacement writes
//! `[superseded by: p31; valid until: DATE]`, so the chain is in the
//! file. Typed links live on the `from` entry's line,
//! `[links: builds_on p7, solves p9]`: the files carry the whole
//! structure, and the sidecar's `memory_links` is rebuilt from them on
//! every sync, like everything else it holds.
//!
//! An entry can cite the files that support it,
//! `[evidence: src/auth.rs@3f2a1c9e0b12]`: a workspace-relative path and
//! the file's git blob id (12 hex digits) when the entry was written.
//! Whether a cited file has changed since is computed on read, never
//! stored: an entry whose evidence moved is flagged for the human and
//! the model, and its ranking is left alone.
//!
//! Superseding an entry flags what rested on it, one hop back: live
//! entries that `builds_on` or `solves` it, and live entries from the
//! same session, gain `[recheck: p12]`. A flag never changes a belief;
//! the human keeps (clears the flag), archives, or edits the entry.

use crate::agent::sanitize_untrusted;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The two memory tiers. Project memory lives in the repo (committable,
/// travels with clones); global memory lives in the user's data dir and
/// follows them across projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Project,
    Global,
}

impl Tier {
    pub fn prefix(self) -> &'static str {
        match self {
            Tier::Project => "p",
            Tier::Global => "g",
        }
    }

    /// Display name, shared with the tools' path grammar.
    pub fn name(self) -> &'static str {
        match self {
            Tier::Project => "project",
            Tier::Global => "global",
        }
    }

    fn from_name(name: &str) -> Tier {
        if name == "global" {
            Tier::Global
        } else {
            Tier::Project
        }
    }
}

/// One parsed entry line, minus its file.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub id: String,
    pub file: String,
    pub text: String,
    pub helpful: u32,
    pub harmful: u32,
    pub source_session: Option<String>,
    pub added_at: Option<String>,
    /// Set by supersede/archive: the entry stopped being current on
    /// this date. History keeps the row and the line.
    pub valid_until: Option<String>,
    /// Set by a supersede that names its replacement: the id that
    /// took this entry's place.
    pub superseded_by: Option<String>,
    /// Typed links from this entry to others, in line order.
    pub links: Vec<Link>,
    /// The files that supported this entry when it was written.
    pub evidence: Vec<Evidence>,
    /// Superseded entries this one rested on: each correction flags
    /// its dependents for a human look, until they are kept or retired.
    pub recheck: Vec<String>,
}

/// One cited file: a workspace-relative path (`/`-separated) and its
/// git blob id, shortened to [`EVIDENCE_HASH_LEN`] hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub path: String,
    pub hash: String,
}

/// Hex digits kept of an evidence hash: enough that a collision
/// between two states of one file is not a practical concern, short
/// enough to keep the line readable, and still a valid abbreviation
/// for `git log --find-object`.
pub const EVIDENCE_HASH_LEN: usize = 12;

/// How many files one entry may cite.
pub const EVIDENCE_LIMIT: usize = 8;

/// The git blob id of `bytes` (what `git hash-object` prints),
/// shortened. Computed here, so it works in a workspace that is not a
/// git repository too.
pub fn blob_hash(bytes: &[u8]) -> String {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    let mut hex = hasher.digest().to_string();
    hex.truncate(EVIDENCE_HASH_LEN);
    hex
}

/// A typed link from one entry to another, stored on the `from`
/// entry's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub relation: String,
    pub to: String,
}

/// The relations a link may carry. Anything else is refused at the
/// op, and a hand-written unknown relation leaves the bracket as
/// entry text (the parser's rule for brackets it does not own).
pub const RELATIONS: [&str; 5] = ["relates_to", "solves", "causes", "builds_on", "replaces"];

/// A search hit: enough to find the entry (the model reads the file),
/// never the whole store.
#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub id: String,
    /// Set when the hit was superseded: the entry is history, and the
    /// model should prefer this successor instead (CUPMem's pattern:
    /// stale knowledge retrieved with a pointer beats stale knowledge
    /// retrieved bare, and beats it not being retrieved at all).
    pub superseded_by: Option<String>,
    pub tier: Tier,
    pub file: String,
    pub text: String,
    pub added_at: Option<String>,
    /// The hit's typed links, so the model can follow them by id.
    pub links: Vec<Link>,
    /// Cited files that changed (or vanished) since the entry was
    /// written. Non-empty means: re-check before relying on it.
    pub evidence_changed: Vec<String>,
    /// Corrected entries this hit rested on (see [`MemoryEntry::recheck`]).
    pub recheck: Vec<String>,
}

/// A delta operation, the only way memory changes under harness hands.
/// The set is deliberately small: every op is a line-level edit, so a
/// merge can never regenerate a file wholesale (the context-collapse
/// guardrail, enforced by construction).
#[derive(Debug, Clone)]
pub enum Op {
    /// Append one entry to the project tier. Creates the topic file if
    /// absent; global adds go through [`add_global`].
    Add {
        file: String,
        text: String,
        source: Option<String>,
        /// Workspace-relative paths that support the entry; each is
        /// hashed as it is written. Paths that do not resolve to a
        /// file inside the workspace are dropped.
        evidence: Vec<String>,
    },
    /// Replace an entry's text, counters and provenance preserved.
    Update { id: String, text: String },
    /// Mark an entry replaced by newer knowledge. `by` names the
    /// replacing entry; None inside a batch with exactly one Add
    /// resolves to that add's new id (extraction's add-then-supersede
    /// pair), otherwise the line records only the date.
    Supersede { id: String, by: Option<String> },
    /// Mark an entry retired (no replacement).
    Archive { id: String },
    /// Consolidation: two entries say the same thing. The first id
    /// survives with `text`; the second is archived and its provenance
    /// rides the survivor's `[source: ...]` list. Delta-only like
    /// everything else: no file is ever regenerated.
    Merge {
        keep: String,
        drop: String,
        text: String,
    },
    /// A typed link between entries, written to the `from` entry's
    /// line (the sidecar rebuilds from it).
    Link {
        from: String,
        to: String,
        relation: String,
    },
}

/// What one `apply` did. Skipped ops are reported, never silent: the
/// caller shows the model what landed and what didn't.
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    /// Per supersede that flagged something: the superseded id and
    /// the entries now marked to recheck.
    pub flagged: Vec<(String, Vec<String>)>,
}

pub struct MemoryStore {
    project_root: PathBuf,
    global_root: PathBuf,
    store: Arc<crate::SessionStore>,
}

impl MemoryStore {
    /// Roots are the two tier directories (files live directly inside:
    /// `MEMORY.md` plus topic files). The sidecar rides the session
    /// store's database, so one open covers everything.
    pub fn open(
        project_root: PathBuf,
        global_root: PathBuf,
        store: Arc<crate::SessionStore>,
    ) -> Self {
        Self {
            project_root,
            global_root,
            store,
        }
    }

    /// The tier root on disk (the tools resolve paths against it) and
    /// today's date (entries stamp themselves). Small surface, the
    /// alternative was duplicating date logic outside the store.
    pub fn root(&self, tier: Tier) -> PathBuf {
        match tier {
            Tier::Project => self.project_root.clone(),
            Tier::Global => self.global_root.clone(),
        }
    }

    /// The workspace the project tier belongs to, which evidence paths
    /// are relative to: the tier lives at `<workspace>/.gertie/memory`;
    /// any other layout (tests) counts the tier root's parent.
    pub fn workspace_root(&self) -> Option<PathBuf> {
        if self.project_root.ends_with(".gertie/memory") {
            self.project_root.parent()?.parent().map(Path::to_path_buf)
        } else {
            self.project_root.parent().map(Path::to_path_buf)
        }
    }

    /// Hashes the named files as they are now. A path is kept only if
    /// it resolves (symlinks included) to a regular file inside the
    /// workspace; it is stored workspace-relative, `/`-separated.
    /// Duplicates collapse; at most [`EVIDENCE_LIMIT`] survive.
    pub fn evidence_for(&self, paths: &[String]) -> Vec<Evidence> {
        let Some(root) = self
            .workspace_root()
            .and_then(|root| root.canonicalize().ok())
        else {
            return Vec::new();
        };
        let mut evidence: Vec<Evidence> = Vec::new();
        for path in paths {
            let Some(relative) = resolve_evidence(&root, path) else {
                continue;
            };
            if evidence.iter().any(|known| known.path == relative) {
                continue;
            }
            let Ok(bytes) = std::fs::read(root.join(&relative)) else {
                continue;
            };
            evidence.push(Evidence {
                path: relative,
                hash: blob_hash(&bytes),
            });
            if evidence.len() == EVIDENCE_LIMIT {
                break;
            }
        }
        evidence
    }

    /// The cited files of `entry` whose content no longer matches the
    /// recorded hash, or that are gone. Empty for an entry with no
    /// evidence, and for a retired entry (history is not re-checked).
    pub fn changed_evidence(&self, entry: &MemoryEntry) -> Vec<String> {
        if entry.evidence.is_empty() || entry.valid_until.is_some() {
            return Vec::new();
        }
        let Some(root) = self.workspace_root() else {
            return Vec::new();
        };
        entry
            .evidence
            .iter()
            .filter(|cited| {
                std::fs::read(root.join(&cited.path))
                    .map(|bytes| blob_hash(&bytes) != cited.hash)
                    .unwrap_or(true)
            })
            .map(|cited| cited.path.clone())
            .collect()
    }

    /// Re-records an entry's evidence hashes to the cited files'
    /// current state, clearing the `[evidence changed]` flag. The
    /// re-check is the caller's claim: the fold fires this when the
    /// session's work re-read a cited file while consulting the entry,
    /// so the reread IS the verification and no click or tool call is
    /// owed. Entries whose facts no longer hold should supersede
    /// instead; rehashing is for facts that survived the refactor.
    /// Returns the paths that were refreshed (still-stale cites stay
    /// flagged; an empty result on a fully-current entry is a no-op).
    pub fn reverify_evidence(&self, id: &str) -> rusqlite::Result<Vec<String>> {
        let entry = self
            .scan()
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if entry.evidence.is_empty() || entry.valid_until.is_some() {
            return Ok(Vec::new());
        }
        let Some(root) = self.workspace_root() else {
            return Ok(Vec::new());
        };
        let mut refreshed = Vec::new();
        let mut evidence = entry.evidence.clone();
        for cited in &mut evidence {
            let bytes = match std::fs::read(root.join(&cited.path)) {
                Ok(bytes) => bytes,
                Err(_) => continue, // gone is still stale; keep the flag
            };
            let hash = blob_hash(&bytes);
            if hash != cited.hash {
                cited.hash = hash;
                refreshed.push(cited.path.clone());
            }
        }
        if refreshed.is_empty() {
            return Ok(Vec::new());
        }
        let line = render_line(&MemoryEntry {
            evidence,
            ..entry.clone()
        });
        rewrite_line(self.root(tier_of(&entry.id)).join(&entry.file), id, |_| {
            Ok(line)
        })?;
        Ok(refreshed)
    }

    /// Re-verifies only the cited paths the session actually re-read:
    /// the given paths' hashes re-record, the rest of the entry's
    /// evidence keeps its stale flag. The per-path form of
    /// [`Self::reverify_evidence`], which the automation calls so a
    /// reread of one file doesn't clear the flag on the others.
    pub fn reverify_evidence_paths(&self, id: &str, paths: &[String]) -> Vec<String> {
        let entry = match self.scan().into_iter().find(|entry| entry.id == id) {
            Some(entry) => entry,
            None => return Vec::new(),
        };
        if entry.evidence.is_empty() || entry.valid_until.is_some() {
            return Vec::new();
        }
        let Some(root) = self.workspace_root() else {
            return Vec::new();
        };
        let mut refreshed = Vec::new();
        let mut evidence = entry.evidence.clone();
        for cited in &mut evidence {
            // Not re-read here: the flag stands either way.
            if !paths.contains(&cited.path) {
                continue;
            }
            let bytes = match std::fs::read(root.join(&cited.path)) {
                Ok(bytes) => bytes,
                Err(_) => continue, // gone is still stale; keep the flag
            };
            let hash = blob_hash(&bytes);
            if hash != cited.hash {
                cited.hash = hash;
                refreshed.push(cited.path.clone());
            }
        }
        if refreshed.is_empty() {
            return Vec::new();
        }
        let line = render_line(&MemoryEntry {
            evidence,
            ..entry.clone()
        });
        if rewrite_line(self.root(tier_of(&entry.id)).join(&entry.file), id, |_| {
            Ok(line)
        })
        .is_err()
        {
            return Vec::new();
        }
        refreshed
    }

    /// Wraps raw model text in the entry grammar, one entry per line:
    /// fresh ids, zeroed counters, provenance, date. One scan for the
    /// whole batch, ids minted sequentially against it (a per-line scan
    /// minted the SAME id for every line of a create, none on disk yet).
    pub fn entry_lines(&self, tier: Tier, text: &str) -> rusqlite::Result<Vec<String>> {
        let prefix = tier.prefix();
        let mut next = self
            .scan()
            .iter()
            .filter_map(|entry| entry.id.strip_prefix(prefix)?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let date = self.today()?;
        let mut out = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                out.push(String::new());
                continue;
            }
            let text = sanitize_untrusted(trimmed, 512);
            let id = format!("{prefix}{next}");
            next += 1;
            out.push(render_line(&MemoryEntry {
                id,
                file: String::new(),
                text,
                helpful: 0,
                harmful: 0,
                source_session: None,
                added_at: Some(date.clone()),
                valid_until: None,
                superseded_by: None,
                links: Vec::new(),
                evidence: Vec::new(),
                recheck: Vec::new(),
            }));
        }
        Ok(out)
    }

    /// The memory tool's write normalizer: `before` is the file as it
    /// is on disk (empty for a create), `after` the model's edit of it.
    /// Whatever the model typed, the result is clean entry grammar:
    ///
    /// - unchanged lines, headings, and legacy prose stay verbatim;
    /// - an existing entry the edit changed keeps its id, votes,
    ///   provenance, evidence and flags (update semantics: the model
    ///   may change its text, links and supersede marker, nothing the
    ///   human or the harness owns, so a rewritten hash can never
    ///   launder a stale-evidence flag);
    /// - new text becomes a new entry: its markdown bullet stripped,
    ///   a fresh id, zeroed votes, today's date, and evidence hashed
    ///   by the harness (a cited path that is not a workspace file is
    ///   dropped; the model's own hash is ignored);
    /// - an entry line with a colliding, duplicated or invented id is
    ///   new text too, with a fresh id;
    /// - plain text that replaces an entry line is that entry's new
    ///   text (paired in order); an edit that removes an entry outright
    ///   is refused, because memory never deletes.
    pub fn normalize_edit(
        &self,
        tier: Tier,
        file: &str,
        before: &str,
        after: &str,
    ) -> Result<String, String> {
        let prefix = tier.prefix();
        let before_entries: Vec<MemoryEntry> = before
            .lines()
            .filter_map(|line| parse_entry(line, tier, file))
            .collect();
        let before_lines: std::collections::HashSet<&str> = before.lines().map(str::trim).collect();
        let known: Vec<MemoryEntry> = self.scan();
        let mut next = known
            .iter()
            .map(|entry| entry.id.as_str())
            .chain(before_entries.iter().map(|entry| entry.id.as_str()))
            .filter_map(|id| id.strip_prefix(prefix)?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let today = self.today().map_err(|error| format!("memory: {error}"))?;

        // One slot per output line; new facts are resolved after the
        // walk, once we know which entries the edit removed.
        enum Slot {
            Done(String),
            New(Box<MemoryEntry>, Vec<String>),
        }
        let mut slots: Vec<Slot> = Vec::new();
        let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
        for line in after.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                slots.push(Slot::Done(String::new()));
                continue;
            }
            if let Some(entry) = parse_entry(trimmed, tier, file) {
                let original = before_entries.iter().find(|mine| mine.id == entry.id);
                match original {
                    Some(original) if !used.contains(&entry.id) => {
                        used.insert(entry.id.clone());
                        if before_lines.contains(trimmed) {
                            slots.push(Slot::Done(line.to_string()));
                        } else {
                            slots.push(Slot::Done(render_line(&MemoryEntry {
                                text: sanitize_untrusted(&entry.text, 512),
                                links: entry.links,
                                valid_until: entry.valid_until.or(original.valid_until.clone()),
                                superseded_by: entry
                                    .superseded_by
                                    .or(original.superseded_by.clone()),
                                ..original.clone()
                            })));
                        }
                    }
                    _ => {
                        // An id that lives in another file: retiring it
                        // here would mint a phantom (a new entry born
                        // retired) and leave the real one live.
                        if let Some(elsewhere) = known
                            .iter()
                            .find(|known| known.id == entry.id && known.file != file)
                            && (entry.valid_until.is_some() || entry.superseded_by.is_some())
                        {
                            return Err(format!(
                                "memory: [{}] lives in {}, not {file}. Retire it there, or call \
                                 remember with action supersede and id {}",
                                entry.id, elsewhere.file, entry.id
                            ));
                        }
                        let paths = entry
                            .evidence
                            .iter()
                            .map(|cited| cited.path.clone())
                            .collect();
                        slots.push(Slot::New(Box::new(entry), paths));
                    }
                }
                continue;
            }
            if trimmed.starts_with('#') || before_lines.contains(trimmed) {
                slots.push(Slot::Done(line.to_string()));
                continue;
            }
            let (entry, paths) = loose_entry(trimmed, tier, file)?;
            slots.push(Slot::New(Box::new(entry), paths));
        }

        // Entries the edit dropped: plain text in their place is their
        // new text (in order); anything else would delete history.
        let removed: Vec<&MemoryEntry> = before_entries
            .iter()
            .filter(|entry| !used.contains(&entry.id))
            .collect();
        let fresh = slots
            .iter()
            .filter(|slot| matches!(slot, Slot::New(..)))
            .count();
        if !removed.is_empty() && removed.len() != fresh {
            let ids = removed
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "memory: this edit removes [{ids}], and memory never deletes. To retire an \
                 entry, append [valid until: {today}] to its line; if a newer entry replaces \
                 it, append [superseded by: pN; valid until: {today}] instead"
            ));
        }
        let mut replaced = removed.into_iter();
        let mut out = Vec::with_capacity(slots.len());
        for slot in slots {
            match slot {
                Slot::Done(line) => out.push(line),
                Slot::New(entry, paths) => {
                    let text = sanitize_untrusted(&entry.text, 512);
                    let line = match replaced.next() {
                        Some(original) => render_line(&MemoryEntry {
                            text,
                            ..original.clone()
                        }),
                        None => {
                            // A brand-new fact cannot be born retired.
                            if entry.valid_until.is_some() || entry.superseded_by.is_some() {
                                return Err(format!(
                                    "memory: a new entry cannot carry [valid until] or \
                                     [superseded by]: write the new fact plainly, and retire \
                                     the old one on its own line ({text})"
                                ));
                            }
                            let id = format!("{prefix}{next}");
                            next += 1;
                            render_line(&MemoryEntry {
                                id,
                                file: file.to_string(),
                                text,
                                helpful: 0,
                                harmful: 0,
                                source_session: None,
                                added_at: Some(today.clone()),
                                valid_until: None,
                                superseded_by: None,
                                links: entry.links,
                                evidence: self.evidence_for(&paths),
                                recheck: Vec::new(),
                            })
                        }
                    };
                    out.push(line);
                }
            }
        }
        let mut joined = out.join("\n");
        if after.ends_with('\n') && !joined.is_empty() {
            joined.push('\n');
        }
        Ok(joined)
    }

    pub fn today(&self) -> rusqlite::Result<String> {
        self.store.with_connection(|connection| {
            connection.query_row("SELECT strftime('%Y-%m-%d','now')", [], |row| {
                row.get::<_, String>(0)
            })
        })
    }

    /// Records one vote on an entry: helpful or harmful, +1 on the
    /// named counter. The file line is rewritten (the counters live in
    /// the entry grammar), the sidecar syncs after. This is the
    /// feedback loop's only writer: dreaming archives on these counts,
    /// so an entry the human keeps confirming survives consolidation.
    /// Returns the new (helpful, harmful) pair.
    pub fn vote(&self, id: &str, helpful: bool) -> rusqlite::Result<(u32, u32)> {
        let entry = self
            .scan()
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let (helpful_count, harmful_count) = if helpful {
            (entry.helpful + 1, entry.harmful)
        } else {
            (entry.helpful, entry.harmful + 1)
        };
        let line = render_line(&MemoryEntry {
            helpful: helpful_count,
            harmful: harmful_count,
            ..entry.clone()
        });
        rewrite_line(self.root(tier_of(&entry.id)).join(&entry.file), id, |_| {
            Ok(line.clone())
        })?;
        self.sync()?;
        Ok((helpful_count, harmful_count))
    }

    /// Every parseable entry across both tiers, file order per tier,
    /// project tier first (it outranks the user's global habits).
    /// How many topic files exist across both tiers, entries or not.
    /// The chip's fallback signal: files without entry-grammar lines
    /// still mean "memory exists here".
    pub fn topic_file_count(&self) -> usize {
        topic_files(&self.project_root).len() + topic_files(&self.global_root).len()
    }

    /// A cheap change signal for the topic files: names, sizes and
    /// modification times across both tiers, hashed. A listing and a
    /// stat per file, no reads, so a caller can ask every frame and
    /// rescan only when the answer moves (the files are rewritten in
    /// place, so a directory mtime alone would miss edits).
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for root in [&self.project_root, &self.global_root] {
            for path in topic_files(root) {
                path.hash(&mut hasher);
                if let Ok(meta) = std::fs::metadata(&path) {
                    meta.len().hash(&mut hasher);
                    meta.modified().ok().hash(&mut hasher);
                }
            }
        }
        hasher.finish()
    }

    pub fn scan(&self) -> Vec<MemoryEntry> {
        let mut entries = Vec::new();
        for tier in [Tier::Project, Tier::Global] {
            let root = self.root(tier);
            for path in topic_files(&root) {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let file = file_name(&path);
                for line in text.lines() {
                    if let Some(entry) = parse_entry(line, tier, &file) {
                        entries.push(entry);
                    }
                }
            }
        }
        entries
    }

    /// The next unused numeric id for a tier. Ids live in the files, so
    /// a fresh scan is the whole ledger; nothing is allocated in the db.
    fn next_id(&self, tier: Tier) -> String {
        let prefix = tier.prefix();
        let max = self
            .scan()
            .iter()
            .filter_map(|entry| entry.id.strip_prefix(prefix)?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("{prefix}{}", max + 1)
    }

    /// Brings the sidecar in line with the files: parse only what the
    /// hash says changed, drop rows for files that left. Link rows are
    /// rebuilt whole from the files every time: they are a handful of
    /// rows, and a rebuild can never disagree with the record.
    pub fn sync(&self) -> rusqlite::Result<()> {
        self.migrate_sidecar_links()?;
        self.store.with_connection(|connection| {
            let on_disk: Vec<(Tier, PathBuf, String, Vec<MemoryEntry>)> =
                [Tier::Project, Tier::Global]
                    .iter()
                    .flat_map(|tier| {
                        let root = self.root(*tier);
                        topic_files(&root)
                            .into_iter()
                            .map(|path| {
                                let bytes = std::fs::read(&path).unwrap_or_default();
                                let hash = format!("{:x}", Sha256::digest(&bytes));
                                let file = file_name(&path);
                                let entries = std::fs::read_to_string(&path)
                                    .map(|text| {
                                        text.lines()
                                            .filter_map(|line| parse_entry(line, *tier, &file))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                (*tier, path, hash, entries)
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect();

            let mut known: BTreeMap<String, String> = BTreeMap::new();
            {
                let mut statement = connection.prepare("SELECT path, hash FROM memory_files")?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (path, hash) = row?;
                    known.insert(path, hash);
                }
            }

            for known_path in known.keys() {
                let still_there = on_disk
                    .iter()
                    .any(|(_, path, _, _)| path.display().to_string() == *known_path);
                if still_there {
                    continue;
                }
                // The tier comes from the stored rows themselves, not
                // a guess from the path: a tempdir test root (or any
                // global root without the XDG marker in its path) made
                // the guess say project while the rows say global, the
                // delete matched nothing, and the rename's re-insert
                // fired UNIQUE. file_name is unique per tier root, so
                // tier='global' OR tier='project' is exact.
                let file = file_name(Path::new(known_path));
                connection.execute(
                    "DELETE FROM memories_fts WHERE id IN
                         (SELECT id FROM memories WHERE file = ?1
                          AND tier IN ('global', 'project'))",
                    rusqlite::params![file],
                )?;
                connection.execute(
                    "DELETE FROM memories WHERE file = ?1
                          AND tier IN ('global', 'project')",
                    rusqlite::params![file],
                )?;
                connection.execute("DELETE FROM memory_files WHERE path = ?1", [known_path])?;
            }
            for (tier, path, hash, entries) in &on_disk {
                let path_string = path.display().to_string();
                if known
                    .get(&path_string)
                    .map(|seen| seen == hash)
                    .unwrap_or(false)
                {
                    continue;
                }
                // The FTS index has no triggers; the sync owns both
                // tables' rows for a changed file, stale index rows for
                // superseded ids must not outlive the rewrite.
                connection.execute(
                    "DELETE FROM memories_fts WHERE id IN
                         (SELECT id FROM memories WHERE tier = ?1 AND file = ?2)",
                    rusqlite::params![tier.name(), file_name(path)],
                )?;
                connection.execute(
                    "DELETE FROM memories WHERE tier = ?1 AND file = ?2",
                    rusqlite::params![tier.name(), file_name(path)],
                )?;
                // Same id twice in one file (a hand edit): the last
                // line wins, the insert must not crash on it.
                let mut seen = std::collections::BTreeSet::new();
                let unique: Vec<&MemoryEntry> = entries
                    .iter()
                    .rev()
                    .filter(|entry| seen.insert(entry.id.clone()))
                    .collect();
                for entry in unique {
                    connection.execute(
                        "INSERT INTO memories (id, tier, file, content, helpful, harmful,
                            source_session, added_at, valid_until, superseded_by)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        rusqlite::params![
                            entry.id,
                            tier.name(),
                            entry.file,
                            entry.text,
                            entry.helpful,
                            entry.harmful,
                            entry.source_session,
                            entry.added_at,
                            entry.valid_until,
                            entry.superseded_by,
                        ],
                    )?;
                    connection.execute(
                        "INSERT INTO memories_fts(id, content, tier) VALUES (?1, ?2, ?3)",
                        rusqlite::params![entry.id, entry.text, tier.name()],
                    )?;
                }
                connection.execute(
                    "INSERT INTO memory_files (path, hash) VALUES (?1, ?2)
                     ON CONFLICT(path) DO UPDATE SET hash = excluded.hash",
                    rusqlite::params![path_string, hash],
                )?;
            }

            // Rows whose file left the disk: gone from every table, the
            // index must not recall what no longer exists.

            connection.execute("DELETE FROM memory_links", [])?;
            for (_, _, _, entries) in &on_disk {
                for entry in entries {
                    for link in &entry.links {
                        connection.execute(
                            "INSERT OR IGNORE INTO memory_links (from_id, to_id, relation)
                             VALUES (?1, ?2, ?3)",
                            rusqlite::params![entry.id, link.to, link.relation],
                        )?;
                    }
                }
            }
            Ok(())
        })
    }

    /// Full-text search over current entries: ranked pointers out, the
    /// model reads the file for the body. Superseded and archived
    /// entries never surface; they are history, not recall.
    /// Records that this digest has had its extraction proposal
    /// taken (the once-per-digest gate). One row per digest.
    pub fn mark_extracted(
        &self,
        digest_hash: &str,
        session_id: Option<i64>,
    ) -> rusqlite::Result<()> {
        self.store.with_connection(|connection| {
            connection.execute(
                "INSERT OR IGNORE INTO memory_extracted (digest_hash, session_id) VALUES (?1, ?2)",
                rusqlite::params![digest_hash, session_id],
            )?;
            Ok(())
        })
    }

    /// Whether this digest already went through extraction.
    pub fn extracted(&self, digest_hash: &str) -> rusqlite::Result<bool> {
        self.store.with_connection(|connection| {
            let seen: i64 = connection.query_row(
                "SELECT count(*) FROM memory_extracted WHERE digest_hash = ?1",
                [digest_hash],
                |row| row.get(0),
            )?;
            Ok(seen > 0)
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<MemoryHit>> {
        let Some(query) = fts_query(&sanitize_untrusted(query, 256)) else {
            return Ok(Vec::new());
        };
        self.store
            .with_connection(|connection| {
                let mut statement = connection.prepare(
                    "SELECT memories.id, memories.tier, memories.file, memories.content,
                        memories.added_at, memories.superseded_by
                 FROM memories_fts
                 JOIN memories ON memories.id = memories_fts.id
                 WHERE memories_fts MATCH ?1
                   -- Superseded entries still match, with a pointer to
                   -- their successor: stale knowledge retrieved with an
                   -- address beats stale knowledge retrieved bare, and
                   -- beats it not being retrieved at all (CUPMem).
                   AND (memories.valid_until IS NULL OR memories.superseded_by IS NOT NULL)
                 ORDER BY rank
                 LIMIT ?2",
                )?;
                let rows = statement.query_map(rusqlite::params![query, limit as i64], |row| {
                    Ok(MemoryHit {
                        id: row.get(0)?,
                        tier: Tier::from_name(&row.get::<_, String>(1)?),
                        file: row.get(2)?,
                        text: row.get(3)?,
                        added_at: row.get(4)?,
                        superseded_by: row.get(5)?,
                        links: Vec::new(),
                        evidence_changed: Vec::new(),
                        recheck: Vec::new(),
                    })
                })?;
                let mut hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                let mut links = connection.prepare(
                    "SELECT relation, to_id FROM memory_links WHERE from_id = ?1
                 ORDER BY rowid",
                )?;
                for hit in &mut hits {
                    hit.links = links
                        .query_map([&hit.id], |row| {
                            Ok(Link {
                                relation: row.get(0)?,
                                to: row.get(1)?,
                            })
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                }
                Ok(hits)
            })
            .map(|mut hits: Vec<MemoryHit>| {
                if hits.is_empty() {
                    return hits;
                }
                let entries = self.scan();
                for hit in &mut hits {
                    if let Some(entry) = entries.iter().find(|entry| entry.id == hit.id) {
                        hit.evidence_changed = self.changed_evidence(entry);
                        hit.recheck = entry.recheck.clone();
                    }
                }
                hits
            })
    }

    /// Entries current as of a date (the bi-temporal read): the ids and
    /// lines stay in the files, only the validity moves.
    pub fn valid_entries(&self, as_of: &str) -> rusqlite::Result<Vec<MemoryEntry>> {
        self.store.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, tier, file, content, helpful, harmful, source_session, added_at,
                        valid_until, superseded_by
                 FROM memories
                 WHERE valid_until IS NULL OR valid_until > ?1
                 ORDER BY id",
            )?;
            let rows = statement.query_map([as_of], |row| {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    file: row.get(2)?,
                    text: row.get(3)?,
                    helpful: row.get(4)?,
                    harmful: row.get(5)?,
                    source_session: row.get(6)?,
                    added_at: row.get(7)?,
                    valid_until: row.get(8)?,
                    superseded_by: row.get(9)?,
                    links: Vec::new(),
                    evidence: Vec::new(),
                    recheck: Vec::new(),
                })
            })?;
            rows.collect()
        })
    }

    /// Applies delta ops, budgeted. Each op is one line-level edit; the
    /// budget caps the total text this call may add or rewrite, so a
    /// runaway extraction cannot balloon the store in one pass.
    pub fn apply(&self, ops: &[Op], budget_tokens: usize) -> rusqlite::Result<ApplyReport> {
        self.locked(|| self.apply_unlocked(ops, budget_tokens))
    }

    /// Runs `write` holding the store's write lock: an exclusive flock
    /// on each tier directory, project then global. Ids mint by
    /// scanning the files then writing them, so two writers in the
    /// window (two tabs on one workspace, or the app and a headless
    /// run) could mint the same id. flock locks belong to the open file
    /// description, so this excludes other threads of this process too.
    /// Not reentrant: nothing inside `write` may take it again. Best
    /// effort where the filesystem refuses directory locks.
    pub fn locked<T>(&self, write: impl FnOnce() -> T) -> T {
        let _held: Vec<std::fs::File> = [&self.project_root, &self.global_root]
            .into_iter()
            .filter_map(|root| {
                std::fs::create_dir_all(root).ok()?;
                let handle = std::fs::File::open(root).ok()?;
                handle.lock().ok()?;
                Some(handle)
            })
            .collect();
        write()
    }

    fn apply_unlocked(&self, ops: &[Op], budget_tokens: usize) -> rusqlite::Result<ApplyReport> {
        let mut report = ApplyReport::default();
        let mut spent = 0usize;
        // A supersede that names nothing, in a batch with exactly one
        // add, was replaced by that add (extraction cannot know the
        // new id, so the pair is the reference).
        let lone_add = ops.iter().filter(|op| matches!(op, Op::Add { .. })).count() == 1;
        let mut added: Option<String> = None;
        for op in ops {
            let cost = match op {
                Op::Add { text, .. } | Op::Update { text, .. } => text.len() / 4 + 1,
                Op::Supersede { .. } | Op::Archive { .. } | Op::Link { .. } => 1,
                Op::Merge { text, .. } => text.len() / 4 + 1,
            };
            // Pre-charge: the budget caps the text this call may add or
            // rewrite, so the cap holds even mid-batch.
            if spent + cost > budget_tokens {
                report.skipped.push(describe(op));
                continue;
            }
            let result = match op {
                Op::Add {
                    file,
                    text,
                    source,
                    evidence,
                } => self
                    .add_entry(file, text, source.clone(), evidence)
                    .map(|id| {
                        added = Some(id);
                    }),
                Op::Update { id, text } => self.update_entry(id, text),
                Op::Supersede { id, by } => {
                    let by = by.clone().or_else(|| added.clone().filter(|_| lone_add));
                    self.supersede_entry(id, by.as_deref()).and_then(|()| {
                        let flagged = self.flag_dependents(id, by.as_deref())?;
                        if !flagged.is_empty() {
                            report.flagged.push((id.clone(), flagged));
                        }
                        Ok(())
                    })
                }
                Op::Archive { id } => self.retire_entry(id, "[archived: "),
                Op::Merge { keep, drop, text } => self.merge_entries(keep, drop, text),
                Op::Link { from, to, relation } => self.link_entries(from, to, relation),
            };
            match result {
                Ok(()) => {
                    spent += cost;
                    report.applied.push(describe(op));
                }
                Err(_) => report.skipped.push(describe(op)),
            }
        }
        self.sync()?;
        Ok(report)
    }

    /// Appends one entry to the project tier. The file name is
    /// constrained to a plain topic file inside the tier root: no
    /// traversal, no `MEMORY.md` (the index is derived, not authored).
    fn add_entry(
        &self,
        file: &str,
        text: &str,
        source: Option<String>,
        evidence: &[String],
    ) -> rusqlite::Result<String> {
        let file = sanitize_untrusted(file, 64);
        if !valid_topic_name(&file) {
            return Err(rusqlite::Error::InvalidParameterName("file".into()));
        }
        let text = sanitize_untrusted(text, 512);
        let id = self.next_id(Tier::Project);
        let date = self.today()?;
        // A correction (#44) is the user stating the rule directly:
        // it starts one vote up, ahead of facts the model inferred.
        let helpful = u32::from(source.as_deref() == Some(CORRECTION_SOURCE));
        let line = render_line(&MemoryEntry {
            id: id.clone(),
            file: file.clone(),
            text: text.clone(),
            helpful,
            harmful: 0,
            source_session: source,
            added_at: Some(date),
            valid_until: None,
            superseded_by: None,
            links: Vec::new(),
            evidence: self.evidence_for(evidence),
            recheck: Vec::new(),
        });
        append_line(self.root(Tier::Project).join(&file), &line)?;
        Ok(id)
    }

    fn update_entry(&self, id: &str, text: &str) -> rusqlite::Result<()> {
        let text = sanitize_untrusted(text, 512);
        let entry = self
            .scan()
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let line = render_line(&MemoryEntry {
            text,
            ..entry.clone()
        });
        rewrite_line(self.root(tier_of(&entry.id)).join(&entry.file), id, |_| {
            Ok(line)
        })
    }

    /// Consolidation (the Merge op): `keep` survives with the new
    /// text and both provenance chains; `drop` is archived. Both ids
    /// must exist in the same tier (a cross-tier merge is a decision
    /// for a human editor, not an op).
    fn merge_entries(&self, keep: &str, drop: &str, text: &str) -> rusqlite::Result<()> {
        let text = sanitize_untrusted(text, 512);
        let entries = self.scan();
        let survivor = entries
            .iter()
            .find(|entry| entry.id == keep)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let gone = entries
            .iter()
            .find(|entry| entry.id == drop)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if tier_of(&survivor.id) != tier_of(&gone.id) {
            return Err(rusqlite::Error::InvalidParameterName("tier".into()));
        }
        // Combined provenance: the survivor's sources, then the
        // dropped entry's.
        let mut sources: Vec<String> = Vec::new();
        if let Some(source) = &survivor.source_session {
            sources.push(source.clone());
        }
        if let Some(source) = &gone.source_session {
            sources.push(source.clone());
        }
        let mut evidence = survivor.evidence.clone();
        for cited in &gone.evidence {
            if evidence.len() < EVIDENCE_LIMIT
                && !evidence.iter().any(|known| known.path == cited.path)
            {
                evidence.push(cited.clone());
            }
        }
        let merged = render_line(&MemoryEntry {
            text,
            evidence,
            helpful: survivor.helpful.max(gone.helpful),
            harmful: survivor.harmful + gone.harmful,
            source_session: if sources.is_empty() {
                None
            } else {
                Some(sources.join(", "))
            },
            ..survivor.clone()
        });
        rewrite_line(
            self.root(tier_of(&survivor.id)).join(&survivor.file),
            keep,
            |_| Ok(merged),
        )?;
        self.retire_entry(drop, "[archived: ")
    }

    /// Supersede, naming the replacement when there is one. The
    /// replacement must exist and must not be the entry itself; an
    /// already-retired entry keeps its first retirement.
    fn supersede_entry(&self, id: &str, by: Option<&str>) -> rusqlite::Result<()> {
        let Some(by) = by else {
            return self.retire_entry(id, "[valid until: ");
        };
        let entries = self.scan();
        if by == id || !entries.iter().any(|entry| entry.id == by) {
            return Err(rusqlite::Error::InvalidParameterName("by".into()));
        }
        let entry = entries
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if entry.valid_until.is_some() {
            return Ok(());
        }
        let line = render_line(&MemoryEntry {
            valid_until: Some(self.today()?),
            superseded_by: Some(by.to_string()),
            ..entry.clone()
        });
        rewrite_line(self.root(tier_of(&entry.id)).join(&entry.file), id, |_| {
            Ok(line)
        })
    }

    /// The correction walk, one hop: after `id` is superseded, every
    /// live entry that `builds_on` or `solves` it, or that shares one
    /// of its source sessions, gains `id` in its `[recheck: ...]`
    /// marker. The replacement itself is never flagged. Returns the
    /// flagged ids, in file order.
    fn flag_dependents(&self, id: &str, by: Option<&str>) -> rusqlite::Result<Vec<String>> {
        let entries = self.scan();
        let Some(corrected) = entries.iter().find(|entry| entry.id == id) else {
            return Ok(Vec::new());
        };
        let sessions = source_sessions(corrected);
        let mut flagged = Vec::new();
        for entry in &entries {
            if entry.id == id
                || Some(entry.id.as_str()) == by
                || entry.valid_until.is_some()
                || entry.recheck.iter().any(|seen| seen == id)
            {
                continue;
            }
            let rests_on = entry.links.iter().any(|link| {
                link.to == id && (link.relation == "builds_on" || link.relation == "solves")
            });
            let sibling = source_sessions(entry)
                .iter()
                .any(|session| sessions.contains(session));
            if !rests_on && !sibling {
                continue;
            }
            let mut recheck = entry.recheck.clone();
            recheck.push(id.to_string());
            let line = render_line(&MemoryEntry {
                recheck,
                ..entry.clone()
            });
            rewrite_line(
                self.root(tier_of(&entry.id)).join(&entry.file),
                &entry.id,
                |_| Ok(line),
            )?;
            flagged.push(entry.id.clone());
        }
        Ok(flagged)
    }

    /// Keep: the human looked at a flagged entry and it still holds.
    /// Clears its `[recheck: ...]` marker; the belief is unchanged.
    pub fn clear_recheck(&self, id: &str) -> rusqlite::Result<()> {
        let entry = self
            .scan()
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if entry.recheck.is_empty() {
            return Ok(());
        }
        let line = render_line(&MemoryEntry {
            recheck: Vec::new(),
            ..entry.clone()
        });
        rewrite_line(self.root(tier_of(&entry.id)).join(&entry.file), id, |_| {
            Ok(line)
        })?;
        self.sync()
    }

    /// Writes a typed link onto the `from` entry's line. Both ends
    /// must exist, the relation must be one of [`RELATIONS`]
    /// (case-folded), and a link already on the line is a no-op.
    fn link_entries(&self, from: &str, to: &str, relation: &str) -> rusqlite::Result<()> {
        let relation = relation.trim().to_ascii_lowercase();
        if !RELATIONS.contains(&relation.as_str()) {
            return Err(rusqlite::Error::InvalidParameterName("relation".into()));
        }
        let entries = self.scan();
        if from == to || !entries.iter().any(|entry| entry.id == to) {
            return Err(rusqlite::Error::InvalidParameterName("to".into()));
        }
        let entry = entries
            .into_iter()
            .find(|entry| entry.id == from)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let link = Link {
            relation,
            to: to.to_string(),
        };
        if entry.links.contains(&link) {
            return Ok(());
        }
        let mut links = entry.links.clone();
        links.push(link);
        let line = render_line(&MemoryEntry {
            links,
            ..entry.clone()
        });
        rewrite_line(
            self.root(tier_of(&entry.id)).join(&entry.file),
            from,
            |_| Ok(line),
        )
    }

    /// The one-time move of links that lived only in the sidecar
    /// (before they were written to the files) onto their `from`
    /// lines. Rows whose ends no longer exist, or with an unknown
    /// relation, are dropped: the files are the record from here on.
    fn migrate_sidecar_links(&self) -> rusqlite::Result<()> {
        type Row = (String, String, String);
        let rows: Option<Vec<Row>> =
            self.store
                .with_connection(|connection| -> rusqlite::Result<Option<Vec<Row>>> {
                    let done: i64 = connection.query_row(
                        "SELECT count(*) FROM memory_migrations WHERE name = 'links_to_files'",
                        [],
                        |row| row.get(0),
                    )?;
                    if done > 0 {
                        return Ok(None);
                    }
                    let mut statement =
                        connection.prepare("SELECT from_id, to_id, relation FROM memory_links")?;
                    let rows = statement
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(Some(rows))
                })?;
        let Some(rows) = rows else {
            return Ok(());
        };
        for (from, to, relation) in rows {
            // A refused row is exactly the case the doc above drops.
            let _ = self.link_entries(&from, &to, &relation);
        }
        self.store.with_connection(|connection| {
            connection.execute(
                "INSERT OR IGNORE INTO memory_migrations (name) VALUES ('links_to_files')",
                [],
            )?;
            Ok(())
        })
    }

    fn retire_entry(&self, id: &str, marker: &str) -> rusqlite::Result<()> {
        let entry = self
            .scan()
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let date = self.today()?;
        let suffix = format!("{marker}{date}]");
        rewrite_line(
            self.root(tier_of(&entry.id)).join(&entry.file),
            id,
            |line| {
                if line.contains("[valid until:") || line.contains("[archived:") {
                    Ok(line.to_string())
                } else {
                    Ok(format!("{line} {suffix}"))
                }
            },
        )
    }

    /// The index the model sees at session start: valid entries grouped
    /// by file, capped whole-file at a time, never truncated mid-entry.
    /// The five canonical sections come first in priority order; any
    /// other topic file follows alphabetically, so a custom file is
    /// indexed too, just last in line for the budget.
    pub fn render_index(&self, budget_tokens: usize) -> String {
        let cost = |line: &str| line.len() / 4 + 1;
        let mut sections: BTreeMap<String, Vec<(i64, u32, String)>> = BTreeMap::new();
        let mut tiers: BTreeMap<String, Tier> = BTreeMap::new();
        let mut any_changed = false;
        for entry in self.scan() {
            if entry.valid_until.is_some() {
                continue;
            }
            let mut mark = String::new();
            if !self.changed_evidence(&entry).is_empty() {
                any_changed = true;
                mark.push_str(" [evidence changed]");
            }
            if !entry.recheck.is_empty() {
                any_changed = true;
                mark.push_str(&format!(" [recheck: {}]", entry.recheck.join(", ")));
            }
            tiers.insert(entry.file.clone(), tier_of(&entry.id));
            sections.entry(entry.file.clone()).or_default().push((
                entry.helpful as i64 - entry.harmful as i64,
                entry.id[1..].parse().unwrap_or(0),
                format!("- [{}] {}{mark}", entry.id, entry.text),
            ));
        }
        // Within a file, the strongest first: votes, then the newest.
        for lines in sections.values_mut() {
            lines.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        }
        let priority = [
            "workflows.md",
            "preferences.md",
            "decisions.md",
            "conventions.md",
            "gotchas.md",
        ];
        let mut order: Vec<&String> = priority
            .iter()
            .filter_map(|file| sections.get_key_value(*file))
            .map(|(key, _)| key)
            .collect();
        for file in sections.keys() {
            if !priority.contains(&file.as_str()) {
                order.push(file);
            }
        }
        // Every file has a one-line form: its name, its size, and its
        // heading, the topic in the author's own words. Room for that
        // line is reserved for every file still to come, so the index
        // is a map of the whole store, never a silent truncation.
        let summary = |file: &str| -> String {
            let count = sections[file].len();
            let heading = tiers
                .get(file)
                .and_then(|tier| std::fs::read_to_string(self.root(*tier).join(file)).ok())
                .and_then(|text| {
                    text.lines().find_map(|line| {
                        line.strip_prefix("# ")
                            .map(|title| title.trim().to_string())
                    })
                })
                .map(|title| format!(": {}", sanitize_untrusted(&title, 120)))
                .unwrap_or_default();
            format!(
                "- {file} ({count} entr{}){heading}",
                if count == 1 { "y" } else { "ies" }
            )
        };
        let summaries: Vec<String> = order.iter().map(|file| summary(file)).collect();
        let mut reserve: usize = summaries.iter().map(|line| cost(line)).sum::<usize>() + 12;

        let mut index = String::from("# Memory\n");
        let mut spent = 3usize; // the header
        if any_changed {
            // One line, only when it applies: the mark's meaning.
            index.push_str(
                "\nEntries marked [evidence changed] cite files that changed since, and \
                 [recheck: id] ones rested on an entry since corrected; re-check \
                 before relying on them.\n",
            );
            spent += 20;
        }
        let mut elsewhere: Vec<&str> = Vec::new();
        for (file, line_of_file) in order.iter().zip(&summaries) {
            reserve -= cost(line_of_file);
            let lines = &sections[file.as_str()];
            let room = budget_tokens.saturating_sub(spent + reserve);
            let head = format!("\n## {file}\n");
            let mut used = cost(&head);
            let mut shown = 0usize;
            for (_, _, line) in lines {
                // A partial file keeps room for its "more" line.
                let tail = if shown + 1 < lines.len() { 12 } else { 0 };
                if used + cost(line) + tail > room {
                    break;
                }
                used += cost(line);
                shown += 1;
            }
            if shown == 0 {
                elsewhere.push(line_of_file);
                spent += cost(line_of_file);
                continue;
            }
            index.push_str(&head);
            for (_, _, line) in lines.iter().take(shown) {
                index.push_str(line);
                index.push('\n');
            }
            if shown < lines.len() {
                let more = format!(
                    "- ({} more in {file}: search_memory, or memory view)\n",
                    lines.len() - shown
                );
                used += cost(&more);
                index.push_str(&more);
            }
            spent += used;
        }
        if !elsewhere.is_empty() {
            index.push_str("\n## Also in memory (search_memory or memory view to read)\n");
            for line in elsewhere {
                index.push_str(line);
                index.push('\n');
            }
        }
        index
    }
}

/// Words that carry no meaning in a memory query; matching them would
/// rank every entry.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "does", "for", "from", "how", "in",
    "is", "it", "of", "on", "or", "should", "that", "the", "this", "to", "was", "we", "what",
    "when", "where", "which", "why", "with", "you",
];

/// A model's natural-language query as an FTS5 expression: each word
/// a quoted prefix term (so no word is ever parsed as syntax), common
/// endings cut so `signing` finds `signed`, joined with OR and left to
/// bm25 to rank: the entry matching most words, rarest first, wins.
/// None when nothing searchable is left.
fn fts_query(raw: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in raw
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.len() > 1 && !STOPWORDS.contains(&word.as_str()))
    {
        let mut stem = word.as_str();
        for suffix in ["ing", "ed", "es", "s"] {
            if let Some(cut) = stem.strip_suffix(suffix)
                && cut.chars().count() >= 3
            {
                stem = cut;
                break;
            }
        }
        let term = format!("\"{stem}\"*");
        if !terms.contains(&term) {
            terms.push(term);
        }
        if terms.len() == 16 {
            break;
        }
    }
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// The source marker of an entry that records a user correction
/// (#44), set by session-end extraction.
pub const CORRECTION_SOURCE: &str = "correction";

/// One lint finding (#38): where, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFinding {
    pub file: String,
    /// 1-based.
    pub line: usize,
    pub message: String,
}

/// Lints one tier's topic files: every shape that hides a fact from
/// the store (prose, malformed lines, the old double-wrap) or breaks
/// its structure (duplicate ids, links to nothing, cited files that
/// are gone). Raw files only, no sidecar, so `doctor` can run it on a
/// store nothing has opened. `workspace` resolves evidence paths;
/// None skips that check.
pub fn lint_tier(root: &Path, tier: Tier, workspace: Option<&Path>) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let mut seen: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let mut parsed: Vec<(String, usize, MemoryEntry)> = Vec::new();
    for path in topic_files(root) {
        let file = file_name(&path);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut finding = |line: usize, message: String| {
            findings.push(LintFinding {
                file: file.clone(),
                line,
                message,
            })
        };
        for (index, raw) in text.lines().enumerate() {
            let number = index + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some(entry) = parse_entry(line, tier, &file) else {
                if line.starts_with("- [") {
                    finding(
                        number,
                        "looks like an entry but does not parse (an entry is `- [pN] helpful=N \
                         harmful=M text [markers]`); fix the line or rewrite it with the \
                         remember tool"
                            .into(),
                    );
                } else {
                    finding(
                        number,
                        "prose: the index, search and voting cannot see it; make it an entry \
                         (remember add, or a `- ` line through the memory tool)"
                            .into(),
                    );
                }
                continue;
            };
            if entry.text.starts_with("- [") || has_counters(&entry.text) {
                finding(
                    number,
                    format!(
                        "[{}] is wrapped twice (an entry inside an entry, the pre-fix tool \
                         path); keep one id and one set of markers",
                        entry.id
                    ),
                );
            }
            if entry.text.starts_with('#') {
                finding(
                    number,
                    format!(
                        "[{}] is a heading stored as an entry; make it a plain `# ` line",
                        entry.id
                    ),
                );
            }
            match seen.get(&entry.id) {
                Some((other, at)) => finding(
                    number,
                    format!(
                        "duplicate id {} (also {other}:{at}); give one of them a fresh id",
                        entry.id
                    ),
                ),
                None => {
                    seen.insert(entry.id.clone(), (file.clone(), number));
                }
            }
            if let Some(workspace) = workspace {
                for cited in &entry.evidence {
                    if !workspace.join(&cited.path).is_file() {
                        finding(
                            number,
                            format!(
                                "[{}] cites {}, which is gone; re-check the fact and supersede \
                                 or archive it",
                                entry.id, cited.path
                            ),
                        );
                    }
                }
            }
            parsed.push((file.clone(), number, entry));
        }
    }
    // References need the whole tier first. Ids of the other tier are
    // out of reach here, so only this tier's prefix is checked.
    for (file, number, entry) in &parsed {
        let mut targets: Vec<(&str, &str)> = entry
            .links
            .iter()
            .map(|link| ("links to", link.to.as_str()))
            .chain(
                entry
                    .recheck
                    .iter()
                    .map(|id| ("is flagged by", id.as_str())),
            )
            .collect();
        if let Some(by) = entry.superseded_by.as_deref().filter(|by| !by.is_empty()) {
            targets.push(("is superseded by", by));
        }
        for (relation, target) in targets {
            if target.starts_with(tier.prefix()) && !seen.contains_key(target) {
                findings.push(LintFinding {
                    file: file.clone(),
                    line: *number,
                    message: format!(
                        "[{}] {relation} {target}, which no entry has; fix the id or drop the \
                         marker",
                        entry.id
                    ),
                });
            }
        }
    }
    findings.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    findings
}

/// Whether text carries real counters (`helpful=3 harmful=0`), the
/// mark of an entry wrapped inside another; a fact that merely
/// describes the grammar (`helpful=N`) does not.
fn has_counters(text: &str) -> bool {
    text.match_indices("helpful=").any(|(at, _)| {
        let rest = &text[at + "helpful=".len()..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && rest[digits..].trim_start().starts_with("harmful=")
    })
}

/// The entries a topic file's text holds, as (id, line): what a write
/// can report back so the model knows exactly what landed.
pub fn entries_in(text: &str, tier: Tier) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            parse_entry(line, tier, "").map(|entry| (entry.id, line.trim().to_string()))
        })
        .collect()
}

/// Adds to the global tier (the user's cross-project posture). Same
/// grammar, other root, own id sequence.
pub fn add_global(
    store: &MemoryStore,
    file: &str,
    text: &str,
    source: Option<String>,
) -> rusqlite::Result<()> {
    let file = sanitize_untrusted(file, 64);
    if !valid_topic_name(&file) {
        return Err(rusqlite::Error::InvalidParameterName("file".into()));
    }
    let text = sanitize_untrusted(text, 512);
    let id = store.next_id(Tier::Global);
    let date = store.today()?;
    let line = render_line(&MemoryEntry {
        id,
        file: file.clone(),
        text,
        helpful: 0,
        harmful: 0,
        source_session: source,
        added_at: Some(date),
        valid_until: None,
        superseded_by: None,
        links: Vec::new(),
        // Global entries span projects: no one workspace to cite.
        evidence: Vec::new(),
        recheck: Vec::new(),
    });
    append_line(store.global_root.join(&file), &line)
}

fn tier_of(id: &str) -> Tier {
    if id.starts_with('g') {
        Tier::Global
    } else {
        Tier::Project
    }
}

fn valid_topic_name(file: &str) -> bool {
    !file.is_empty()
        && file.ends_with(".md")
        && !file.contains('/')
        && !file.contains('\\')
        && file != "MEMORY.md"
}

fn topic_files(root: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("md")
                && path.file_name().and_then(|n| n.to_str()) != Some("MEMORY.md")
        })
        .collect();
    files.sort();
    files
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

/// Parses one entry line. Unparsable lines return None and are left
/// exactly where they are: prose, headings, and future formats survive
/// a round trip untouched.
fn parse_entry(line: &str, tier: Tier, file: &str) -> Option<MemoryEntry> {
    let rest = line.trim_start().strip_prefix("- [")?;
    let close = rest.find(']')?;
    let id = rest[..close].trim().to_string();
    if id.len() < 2
        || !id.starts_with(tier.prefix())
        || !id[1..].chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let after = rest[close + 1..].trim_start();
    // Counters: `helpful=N harmful=M` (order fixed, either may be 0).
    let counters = after.strip_prefix("helpful=")?;
    let helpful_end = counters.find(' ')?;
    let helpful: u32 = counters[..helpful_end].parse().ok()?;
    let rest_counters = counters[helpful_end..]
        .trim_start()
        .strip_prefix("harmful=")?;
    let harmful_end = rest_counters.find(' ')?;
    let harmful: u32 = rest_counters[..harmful_end].parse().ok()?;
    let body = rest_counters[harmful_end..].trim_start();

    // Trailing markers are peeled from the end: each trailing
    // `[...]` bracket is either a known marker (source/added,
    // valid until, archived) or part of the entry text, in which
    // case the peel stops and the bracket stays in the text. This
    // makes marker order free and keeps prose with brackets intact.
    let mut text = body.to_string();
    let mut source_session = None;
    let mut added_at = None;
    let mut valid_until = None;
    let mut superseded_by = None;
    let mut links = Vec::new();
    let mut evidence = Vec::new();
    let mut recheck = Vec::new();
    loop {
        let trimmed = text.trim_end();
        if !trimmed.ends_with(']') {
            break;
        }
        let Some(open) = trimmed.rfind('[') else {
            break;
        };
        let inner = &trimmed[open + 1..trimmed.len() - 1];
        if let Some(date) = inner.strip_prefix("valid until:") {
            valid_until = Some(date.trim().to_string());
        } else if inner.starts_with("superseded by:") {
            for part in inner.split(';') {
                let part = part.trim();
                if let Some(by) = part.strip_prefix("superseded by:") {
                    superseded_by = Some(by.trim().to_string());
                } else if let Some(date) = part.strip_prefix("valid until:") {
                    valid_until = Some(date.trim().to_string());
                }
            }
        } else if let Some(list) = inner.strip_prefix("links:")
            && let Some(parsed) = parse_links(list)
        {
            links = parsed;
        } else if let Some(list) = inner.strip_prefix("evidence:")
            && let Some(parsed) = parse_evidence(list)
        {
            evidence = parsed;
        } else if let Some(list) = inner.strip_prefix("recheck:")
            && let Some(parsed) = parse_ids(list)
        {
            recheck = parsed;
        } else if let Some(date) = inner.strip_prefix("archived:") {
            valid_until = Some(date.trim().to_string());
        } else if inner.starts_with("source:") || inner.starts_with("added:") {
            for part in inner.split(';') {
                let part = part.trim();
                // `[source: added: D]` is what the writer emits for an
                // entry with no session: the date, not a source.
                let part = match part.strip_prefix("source:").map(str::trim) {
                    Some(rest) if rest.starts_with("added:") => rest,
                    Some(source) => {
                        source_session = Some(source.to_string());
                        continue;
                    }
                    None => part,
                };
                if let Some(added) = part.strip_prefix("added:") {
                    added_at = Some(added.trim().to_string());
                }
            }
        } else {
            break;
        }
        text = trimmed[..open].trim_end().to_string();
    }
    if text.is_empty() {
        return None;
    }
    Some(MemoryEntry {
        id,
        file: file.to_string(),
        text,
        helpful,
        harmful,
        source_session,
        added_at,
        valid_until,
        superseded_by,
        links,
        evidence,
        recheck,
    })
}

/// A line of new model text as an entry: the markdown bullet and any
/// half-written grammar (a stray `[p3]`, `helpful=N harmful=M`) are
/// stripped, the trailing markers peel as they do for a real line, and
/// a bare `[evidence: a.rs, b.rs]` (paths without hashes) is accepted,
/// since the harness hashes anyway. Returns the entry and the cited
/// paths.
fn loose_entry(line: &str, tier: Tier, file: &str) -> Result<(MemoryEntry, Vec<String>), String> {
    let mut text = line.trim();
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = text.strip_prefix(bullet) {
            text = rest.trim_start();
            break;
        }
    }
    if let Some(rest) = text.strip_prefix('[')
        && let Some((id, after)) = rest.split_once(']')
        && id.len() >= 2
        && (id.starts_with('p') || id.starts_with('g'))
        && id[1..].chars().all(|c| c.is_ascii_digit())
    {
        text = after.trim_start();
    }
    if let Some(rest) = text.strip_prefix("helpful=")
        && let Some((_, after)) = rest.split_once(' ')
        && let Some(rest) = after.strip_prefix("harmful=")
        && let Some((_, after)) = rest.split_once(' ')
    {
        text = after.trim_start();
    }
    // Bare evidence (no hashes) would stay in the text; lift it out.
    let mut paths = Vec::new();
    let mut body = text.to_string();
    if let Some(open) = body.find("[evidence:")
        && let Some(close) = body[open..].find(']')
    {
        let inner = &body[open + "[evidence:".len()..open + close];
        if !inner.contains('@') {
            paths = inner
                .split(',')
                .map(|path| path.trim().to_string())
                .filter(|path| !path.is_empty())
                .collect();
            body = format!("{}{}", &body[..open], &body[open + close + 1..])
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
    let candidate = format!("- [{}0] helpful=0 harmful=0 {body}", tier.prefix());
    let entry = parse_entry(&candidate, tier, file)
        .ok_or_else(|| format!("memory: nothing to remember in {line:?}"))?;
    paths.extend(entry.evidence.iter().map(|cited| cited.path.clone()));
    Ok((entry, paths))
}

/// `p12, g3` into ids; None (the bracket stays text) when any item is
/// not an entry id.
fn parse_ids(list: &str) -> Option<Vec<String>> {
    list.split(',')
        .map(|item| {
            let id = item.trim();
            let valid = id.len() >= 2
                && (id.starts_with('p') || id.starts_with('g'))
                && id[1..].chars().all(|c| c.is_ascii_digit());
            valid.then(|| id.to_string())
        })
        .collect()
}

/// An entry's source sessions: a merge joins them with ", ".
/// One hop of a correction walk, the reason a `[recheck: ...]` flag
/// exists: `corrected` was superseded (by `replaced_by`, when the
/// supersede named one), and `flagged` rested on it through
/// `relation` (`builds_on`, `solves`, or a shared source session).
/// The walk is only as good as its model-proposed edges; showing it
/// makes a wrong edge auditable instead of a mystery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorrectionStep {
    pub corrected: String,
    pub replaced_by: Option<String>,
    pub relation: String,
    pub flagged: String,
}

impl CorrectionStep {
    /// The walk in one sentence: `flagged because p12 was corrected
    /// (replaced by p31); p20 builds_on p12`.
    pub fn sentence(&self) -> String {
        let replaced = self
            .replaced_by
            .as_deref()
            .map(|by| format!(" (replaced by {by})"))
            .unwrap_or_default();
        format!(
            "flagged because {} was corrected{replaced}; {} {} {}",
            self.corrected, self.flagged, self.relation, self.corrected
        )
    }
}

/// Why `id` carries its recheck flags: one step per corrected entry
/// it names, reconstructed from the files the way `flag_dependents`
/// decided it (a `builds_on`/`solves` link first, a shared source
/// session otherwise). A corrected id no longer on disk still gets a
/// step, with the relation unknown, so the flag never goes
/// unexplained.
pub fn correction_walk(entries: &[MemoryEntry], id: &str) -> Vec<CorrectionStep> {
    let Some(entry) = entries.iter().find(|entry| entry.id == id) else {
        return Vec::new();
    };
    entry
        .recheck
        .iter()
        .map(|corrected_id| {
            let corrected = entries.iter().find(|other| &other.id == corrected_id);
            let relation = entry
                .links
                .iter()
                .find(|link| {
                    &link.to == corrected_id
                        && (link.relation == "builds_on" || link.relation == "solves")
                })
                .map(|link| link.relation.clone())
                .or_else(|| {
                    let theirs = corrected.map(source_sessions).unwrap_or_default();
                    source_sessions(entry)
                        .iter()
                        .any(|session| theirs.contains(session))
                        .then(|| "shares a source session with".to_string())
                })
                .unwrap_or_else(|| "rests on".to_string());
            CorrectionStep {
                corrected: corrected_id.clone(),
                replaced_by: corrected.and_then(|c| c.superseded_by.clone()),
                relation,
                flagged: entry.id.clone(),
            }
        })
        .collect()
}

fn source_sessions(entry: &MemoryEntry) -> Vec<String> {
    entry
        .source_session
        .as_deref()
        .map(|sources| {
            sources
                .split(',')
                .map(|source| source.trim().to_string())
                .filter(|source| !source.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `src/a.rs@3f2a1c9e0b12, b.rs@...` into evidence; None (the bracket
/// stays text) when any item is not `path@hex`.
fn parse_evidence(list: &str) -> Option<Vec<Evidence>> {
    list.split(", ")
        .map(|item| {
            let (path, hash) = item.trim().rsplit_once('@')?;
            if path.is_empty() || hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            Some(Evidence {
                path: path.to_string(),
                hash: hash.to_string(),
            })
        })
        .collect()
}

/// A cited path made workspace-relative and checked: relative or
/// absolute-inside, no escape (symlinks resolved), a regular file.
/// A path holding the list separator, or a bracket, could not round
/// trip through the line, so it is refused.
fn resolve_evidence(root: &Path, path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() || path.contains(", ") || path.contains(']') || path.contains('[') {
        return None;
    }
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let resolved = joined.canonicalize().ok()?;
    if !resolved.starts_with(root) || !resolved.is_file() {
        return None;
    }
    let relative = resolved.strip_prefix(root).ok()?;
    Some(
        relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// `builds_on p7, solves p9` into links; None (the bracket stays
/// text) when any item is not `relation id` with a known relation.
fn parse_links(list: &str) -> Option<Vec<Link>> {
    list.split(',')
        .map(|item| {
            let mut words = item.split_whitespace();
            let relation = words.next()?;
            let to = words.next()?;
            if words.next().is_some() || !RELATIONS.contains(&relation) {
                return None;
            }
            Some(Link {
                relation: relation.to_string(),
                to: to.to_string(),
            })
        })
        .collect()
}

/// Renders an entry back to its canonical line. Parse then render is
/// the identity function (the property the round-trip test holds us to).
fn render_line(entry: &MemoryEntry) -> String {
    let mut line = format!(
        "- [{}] helpful={} harmful={} {}",
        entry.id, entry.helpful, entry.harmful, entry.text
    );
    if !entry.links.is_empty() {
        let list = entry
            .links
            .iter()
            .map(|link| format!("{} {}", link.relation, link.to))
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(" [links: {list}]"));
    }
    if !entry.evidence.is_empty() {
        let list = entry
            .evidence
            .iter()
            .map(|cited| format!("{}@{}", cited.path, cited.hash))
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(" [evidence: {list}]"));
    }
    if !entry.recheck.is_empty() {
        line.push_str(&format!(" [recheck: {}]", entry.recheck.join(", ")));
    }
    if entry.source_session.is_some() || entry.added_at.is_some() {
        line.push_str(" [source: ");
        let mut first = true;
        if let Some(source) = &entry.source_session {
            line.push_str(source);
            first = false;
        }
        if let Some(added) = &entry.added_at {
            if !first {
                line.push_str("; ");
            }
            line.push_str(&format!("added: {added}"));
        }
        line.push(']');
    }
    match (&entry.superseded_by, &entry.valid_until) {
        (Some(by), Some(until)) => {
            line.push_str(&format!(" [superseded by: {by}; valid until: {until}]"))
        }
        (Some(by), None) => line.push_str(&format!(" [superseded by: {by}]")),
        (None, Some(until)) => line.push_str(&format!(" [valid until: {until}]")),
        (None, None) => {}
    }
    line
}

/// Appends one line to a file, creating the tier root and the file on
/// first use. A file that exists but ends without a newline gets one:
/// entries are line-delimited.
fn append_line(path: PathBuf, line: &str) -> rusqlite::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    use std::io::Write;
    let needs_newline = std::fs::read(&path)
        .map(|bytes| !bytes.is_empty() && bytes.last() != Some(&b'\n'))
        .unwrap_or(false);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(io_error)?;
    if needs_newline {
        file.write_all(b"\n").map_err(io_error)?;
    }
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(io_error)
}

/// Rewrites the one line an id owns. The closure decides the new text;
/// every other line is untouchable. The file is rewritten whole (it is
/// small, hundreds of lines at most) but only the matched line may
/// change bytes.
fn rewrite_line<F>(path: PathBuf, id: &str, edit: F) -> rusqlite::Result<()>
where
    F: FnOnce(&str) -> std::io::Result<String>,
{
    let text = std::fs::read_to_string(&path).map_err(io_error)?;
    let needle = format!("- [{id}]");
    let mut edit = Some(edit);
    let mut changed = false;
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        if trimmed.starts_with(&needle) && !changed {
            let new_line = (edit.take().unwrap())(trimmed).map_err(io_error)?;
            out.push_str(&new_line);
            out.push('\n');
            changed = true;
        } else {
            out.push_str(line);
        }
    }
    if !changed {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    std::fs::write(&path, out).map_err(io_error)
}

fn io_error(error: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(1), Some(error.to_string()))
}

fn describe(op: &Op) -> String {
    match op {
        Op::Add { file, text, .. } => format!("add {file}: {}", first_words(text)),
        Op::Update { id, text } => format!("update {id}: {}", first_words(text)),
        Op::Supersede { id, by: Some(by) } => format!("supersede {id} by {by}"),
        Op::Supersede { id, by: None } => format!("supersede {id}"),
        Op::Archive { id } => format!("archive {id}"),
        Op::Merge { keep, drop, .. } => format!("merge {keep} into {drop}"),
        Op::Link { from, to, relation } => format!("link {from} {relation} {to}"),
    }
}

fn first_words(text: &str) -> String {
    let mut words: Vec<&str> = text.split_whitespace().take(5).collect();
    if words.len() == 5 {
        words.push("…");
    }
    words.join(" ")
}

/// The sidecar schema, created by `SessionStore::open`. No triggers:
/// the sync pass maintains `memories_fts` directly, memory writes are
/// batch rewrites, not per-message appends.
pub(crate) fn migrate(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS memories (
             id TEXT PRIMARY KEY,
             tier TEXT NOT NULL,
             file TEXT NOT NULL,
             content TEXT NOT NULL,
             helpful INTEGER NOT NULL DEFAULT 0,
             harmful INTEGER NOT NULL DEFAULT 0,
             source_session TEXT,
             added_at TEXT,
             valid_until TEXT,
             retrieved_at TEXT,
             superseded_by TEXT
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
             USING fts5(content, id UNINDEXED, tier UNINDEXED);
         CREATE TABLE IF NOT EXISTS memory_files (
             path TEXT PRIMARY KEY,
             hash TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS memory_links (
             from_id TEXT NOT NULL,
             to_id TEXT NOT NULL,
             relation TEXT NOT NULL,
             PRIMARY KEY (from_id, to_id, relation)
         );
         -- One-time data moves, by name.
         CREATE TABLE IF NOT EXISTS memory_migrations (
             name TEXT PRIMARY KEY
         );
         -- Extraction's once-per-digest gate: a session whose digest
         -- hash is here has already had its proposal taken.
         CREATE TABLE IF NOT EXISTS memory_extracted (
             digest_hash TEXT PRIMARY KEY,
             session_id INTEGER,
             extracted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
         );",
    )?;
    // Columns added after a table first shipped: CREATE TABLE IF NOT
    // EXISTS leaves an existing table as it was.
    let has_superseded_by = connection
        .prepare("SELECT 1 FROM pragma_table_info('memories') WHERE name = 'superseded_by'")?
        .exists([])?;
    if !has_superseded_by {
        connection.execute("ALTER TABLE memories ADD COLUMN superseded_by TEXT", [])?;
        // Rows synced before the column existed must be re-parsed for
        // it: forgetting the file hashes makes the next sync do that.
        connection.execute("DELETE FROM memory_files", [])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_roots(tmp: &tempfile::TempDir) -> (MemoryStore, Arc<crate::SessionStore>) {
        let store = Arc::new(crate::SessionStore::open(std::path::Path::new(":memory:")).unwrap());
        (
            MemoryStore::open(
                tmp.path().join("project-memory"),
                tmp.path().join("global-memory"),
                store.clone(),
            ),
            store,
        )
    }

    #[test]
    fn the_correction_walk_names_the_edge_behind_a_flag() {
        let entry = |id: &str, source: &str| MemoryEntry {
            id: id.into(),
            file: "notes.md".into(),
            text: format!("entry {id}"),
            helpful: 0,
            harmful: 0,
            source_session: Some(source.into()),
            added_at: None,
            valid_until: None,
            superseded_by: None,
            links: Vec::new(),
            evidence: Vec::new(),
            recheck: Vec::new(),
        };
        let corrected = MemoryEntry {
            superseded_by: Some("p31".into()),
            valid_until: Some("2026-11-26".into()),
            ..entry("p12", "4")
        };
        let linked = MemoryEntry {
            links: vec![Link {
                relation: "builds_on".into(),
                to: "p12".into(),
            }],
            recheck: vec!["p12".into()],
            ..entry("p20", "9")
        };
        let sibling = MemoryEntry {
            recheck: vec!["p12".into(), "p99".into()],
            ..entry("p21", "4")
        };
        let entries = vec![corrected, linked, sibling];

        let walk = correction_walk(&entries, "p20");
        assert_eq!(walk.len(), 1);
        assert_eq!(
            walk[0].sentence(),
            "flagged because p12 was corrected (replaced by p31); p20 builds_on p12"
        );
        let walk = correction_walk(&entries, "p21");
        assert_eq!(walk[0].relation, "shares a source session with");
        assert_eq!(
            walk[1].relation, "rests on",
            "a vanished entry still explains the flag"
        );
        assert!(
            correction_walk(&entries, "p12").is_empty(),
            "no flag, no walk"
        );
    }

    #[test]
    fn scan_probe_sees_a_hand_written_line() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("gotchas.md"),
            "# Gotchas\n\n- [p1] helpful=0 harmful=0 the flake hides in teardown [source: 9; added: 2026-10-31]\n",
        )
        .unwrap();
        let store = MemoryStore::open(
            root,
            tmp.path().join("g"),
            Arc::new(crate::SessionStore::open(std::path::Path::new(":memory:")).unwrap()),
        );
        let scan = store.scan();
        assert_eq!(scan.len(), 1, "scan sees the line: {:?}", scan);
    }

    #[test]
    fn parse_and_render_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("gotchas.md"),
            "# Gotchas\n\n\
             - [p1] helpful=2 harmful=0 cite the flake before blaming the test [source: 388; added: 2026-07-11]\n\
             - [p2] helpful=0 harmful=1 never rm -rf the cache by default\n\
             - [p3] helpful=1 harmful=0 old advice [source: 12; added: 2026-01-01] [valid until: 2026-02-01]\n\
             prose and headings survive untouched\n",
        )
        .unwrap();
        let store = MemoryStore::open(
            root.clone(),
            tmp.path().join("g"),
            Arc::new(crate::SessionStore::open(std::path::Path::new(":memory:")).unwrap()),
        );
        let entries = store.scan();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "p1");
        assert_eq!(entries[0].helpful, 2);
        assert_eq!(entries[0].source_session.as_deref(), Some("388"));
        assert_eq!(entries[0].added_at.as_deref(), Some("2026-07-11"));
        assert_eq!(entries[1].helpful, 0);
        assert_eq!(entries[2].valid_until.as_deref(), Some("2026-02-01"));
        // Round trip: rendering what was parsed reproduces the bytes.
        assert_eq!(
            render_line(&entries[0]),
            "- [p1] helpful=2 harmful=0 cite the flake before blaming the test [source: 388; added: 2026-07-11]"
        );
    }

    /// Two stores on the same roots (two tabs on one workspace) adding
    /// at once must never mint the same id.
    #[test]
    fn concurrent_writers_mint_unique_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let writers: Vec<_> = (0..4)
            .map(|writer| {
                let (store, _session) = store_with_roots(&tmp);
                std::thread::spawn(move || {
                    for n in 0..10 {
                        store
                            .apply(
                                &[Op::Add {
                                    file: "race.md".into(),
                                    text: format!("writer {writer} fact {n}"),
                                    source: None,
                                    evidence: Vec::new(),
                                }],
                                256,
                            )
                            .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let (store, _session) = store_with_roots(&tmp);
        let entries = store.scan();
        assert_eq!(entries.len(), 40, "every add landed: {entries:?}");
        let mut ids: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 40, "unique ids");
    }

    #[test]
    fn apply_adds_updates_supersedes_within_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);

        let report = store
            .apply(
                &[
                    Op::Add {
                        file: "gotchas.md".into(),
                        text: "the flake is the teardown, not the test".into(),
                        source: Some("12".into()),
                        evidence: Vec::new(),
                    },
                    Op::Add {
                        file: "gotchas.md".into(),
                        text: "x".repeat(4096),
                        source: None,
                        evidence: Vec::new(),
                    },
                ],
                256,
            )
            .unwrap();
        assert_eq!(report.applied.len(), 1);
        assert_eq!(
            report.skipped.len(),
            1,
            "the oversized add is skipped, named in the report"
        );

        let entries = store.scan();
        assert_eq!(entries[0].id, "p1");
        assert!(entries[0].text.starts_with("the flake is the teardown"));

        let report = store
            .apply(
                &[
                    Op::Update {
                        id: "p1".into(),
                        text: "the flake is the teardown, always was".into(),
                    },
                    Op::Supersede {
                        id: "p1".into(),
                        by: None,
                    },
                    Op::Archive { id: "p999".into() },
                ],
                4096,
            )
            .unwrap();
        assert_eq!(report.applied.len(), 2);
        assert_eq!(
            report.skipped.len(),
            1,
            "retiring an unknown id is a reported skip"
        );

        // The file line carries the validity marker; the sidecar agrees.
        let file = std::fs::read_to_string(tmp.path().join("project-memory/gotchas.md")).unwrap();
        assert!(file.contains("[valid until: "));
        store.sync().unwrap();
        let hits = store.search("teardown", 10).unwrap();
        assert!(hits.is_empty(), "superseded entries never surface");
        let all = store.valid_entries("9999-12-31").unwrap();
        assert_eq!(
            all.len(),
            0,
            "everything retired: history queryable, recall empty"
        );
    }

    fn link_count(session: &crate::SessionStore) -> i64 {
        session
            .with_connection(|connection| {
                connection.query_row("SELECT count(*) FROM memory_links", [], |row| row.get(0))
            })
            .unwrap()
    }

    fn add(store: &MemoryStore, text: &str) {
        let report = store
            .apply(
                &[Op::Add {
                    file: "decisions.md".into(),
                    text: text.into(),
                    source: None,
                    evidence: Vec::new(),
                }],
                4096,
            )
            .unwrap();
        assert_eq!(report.applied.len(), 1);
    }

    #[test]
    fn sync_rebuilds_from_files_and_links_come_from_the_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, session) = store_with_roots(&tmp);
        add(&store, "postgres over sqlite");
        add(&store, "the pool is sized for postgres");
        let report = store
            .apply(
                &[Op::Link {
                    from: "p2".into(),
                    to: "p1".into(),
                    relation: "BUILDS_ON".into(),
                }],
                16,
            )
            .unwrap();
        assert_eq!(report.applied.len(), 1);
        let path = tmp.path().join("project-memory/decisions.md");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("the pool is sized for postgres [links: builds_on p1]"));

        // A hand edit: the next sync picks it up (the hash changed).
        std::fs::write(
            &path,
            format!("{text}- [p1] helpful=9 harmful=9 tampered [source: 1; added: 2026-01-01]\n"),
        )
        .unwrap();
        store.sync().unwrap();
        let entries = store.scan();
        assert_eq!(entries.len(), 3, "the hand-written line parses too");
        assert_eq!(entries[2].helpful, 9);
        let hits = store.search("tampered", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "p1");
        assert_eq!(link_count(&session), 1);

        // The sidecar is derived: wiped, it comes back from the files,
        // links included, the way a clone's would.
        session
            .with_connection(|connection| {
                connection.execute_batch("DELETE FROM memory_links; DELETE FROM memory_files;")
            })
            .unwrap();
        store.sync().unwrap();
        assert_eq!(link_count(&session), 1);
        let hits = store.search("pool", 10).unwrap();
        assert_eq!(
            hits[0].links,
            vec![Link {
                relation: "builds_on".into(),
                to: "p1".into()
            }]
        );

        // Removing the link from the line removes it from the sidecar.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace(" [links: builds_on p1]", "")).unwrap();
        store.sync().unwrap();
        assert_eq!(link_count(&session), 0);
    }

    #[test]
    fn a_link_needs_two_real_ends_and_a_known_relation() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        add(&store, "one");
        add(&store, "two");
        let link = |from: &str, to: &str, relation: &str| Op::Link {
            from: from.into(),
            to: to.into(),
            relation: relation.into(),
        };
        let report = store
            .apply(
                &[
                    link("p1", "p1", "solves"),
                    link("p1", "p9", "solves"),
                    link("p1", "p2", "inspires"),
                    link("p1", "p2", "solves"),
                    link("p1", "p2", "solves"),
                ],
                4096,
            )
            .unwrap();
        assert_eq!(report.skipped.len(), 3);
        let entries = store.scan();
        assert_eq!(entries[0].links.len(), 1, "a repeated link is a no-op");
    }

    #[test]
    fn a_supersede_names_its_replacement_on_the_line() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, session) = store_with_roots(&tmp);
        add(&store, "old belief");
        add(&store, "new belief");
        let report = store
            .apply(
                &[Op::Supersede {
                    id: "p1".into(),
                    by: Some("p2".into()),
                }],
                16,
            )
            .unwrap();
        assert_eq!(report.applied, vec!["supersede p1 by p2"]);
        let text = std::fs::read_to_string(tmp.path().join("project-memory/decisions.md")).unwrap();
        let today = store.today().unwrap();
        assert!(text.contains(&format!(
            "old belief [source: added: {today}] [superseded by: p2; valid until: {today}]"
        )));
        let entries = store.scan();
        assert_eq!(entries[0].superseded_by.as_deref(), Some("p2"));
        assert_eq!(entries[0].valid_until.as_deref(), Some(today.as_str()));
        // The sidecar carries the pointer too.
        let by: Option<String> = session
            .with_connection(|connection| {
                connection.query_row(
                    "SELECT superseded_by FROM memories WHERE id = 'p1'",
                    [],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(by.as_deref(), Some("p2"));

        // A replacement that does not exist, or the entry itself, is refused.
        add(&store, "third");
        let report = store
            .apply(
                &[
                    Op::Supersede {
                        id: "p3".into(),
                        by: Some("p77".into()),
                    },
                    Op::Supersede {
                        id: "p3".into(),
                        by: Some("p3".into()),
                    },
                ],
                16,
            )
            .unwrap();
        assert_eq!(report.skipped.len(), 2);
    }

    #[test]
    fn a_lone_add_is_what_its_batch_supersede_was_replaced_by() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        add(&store, "use tabs");
        store
            .apply(
                &[
                    Op::Add {
                        file: "decisions.md".into(),
                        text: "use spaces".into(),
                        source: None,
                        evidence: Vec::new(),
                    },
                    Op::Supersede {
                        id: "p1".into(),
                        by: None,
                    },
                ],
                4096,
            )
            .unwrap();
        assert_eq!(store.scan()[0].superseded_by.as_deref(), Some("p2"));

        // Two adds: no guess, the date alone.
        store
            .apply(
                &[
                    Op::Add {
                        file: "decisions.md".into(),
                        text: "a".into(),
                        source: None,
                        evidence: Vec::new(),
                    },
                    Op::Add {
                        file: "decisions.md".into(),
                        text: "b".into(),
                        source: None,
                        evidence: Vec::new(),
                    },
                    Op::Supersede {
                        id: "p2".into(),
                        by: None,
                    },
                ],
                4096,
            )
            .unwrap();
        let p2 = store
            .scan()
            .into_iter()
            .find(|entry| entry.id == "p2")
            .unwrap();
        assert!(p2.valid_until.is_some());
        assert_eq!(p2.superseded_by, None);
    }

    #[test]
    fn the_new_markers_round_trip_and_unknown_links_stay_text() {
        let line = "- [p4] helpful=1 harmful=0 the pool is tuned [links: builds_on p1, solves p2] \
                    [source: 12; added: 2026-11-01] [superseded by: p9; valid until: 2026-11-20]";
        let entry = parse_entry(line, Tier::Project, "decisions.md").unwrap();
        assert_eq!(entry.text, "the pool is tuned");
        assert_eq!(entry.links.len(), 2);
        assert_eq!(entry.superseded_by.as_deref(), Some("p9"));
        assert_eq!(entry.valid_until.as_deref(), Some("2026-11-20"));
        assert_eq!(render_line(&entry), line);

        let odd = "- [p5] helpful=0 harmful=0 see [links: inspired p1]";
        let entry = parse_entry(odd, Tier::Project, "decisions.md").unwrap();
        assert_eq!(entry.text, "see [links: inspired p1]");
        assert!(entry.links.is_empty());
    }

    #[test]
    fn the_evidence_hash_is_gits_blob_id() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(blob_hash(b"hello\n"), "ce013625030b");
        assert_eq!(blob_hash(b""), "e69de29bb2d1");
    }

    fn add_with_evidence(store: &MemoryStore, text: &str, evidence: &[&str]) -> ApplyReport {
        store
            .apply(
                &[Op::Add {
                    file: "decisions.md".into(),
                    text: text.into(),
                    source: None,
                    evidence: evidence.iter().map(|path| path.to_string()).collect(),
                }],
                4096,
            )
            .unwrap()
    }

    #[test]
    fn an_entry_cites_its_files_and_knows_when_they_change() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/pool.rs"), "const SIZE: u32 = 8;\n").unwrap();
        std::fs::write(tmp.path().join("README.md"), "readme\n").unwrap();
        add_with_evidence(
            &store,
            "the pool holds eight connections",
            &[
                "src/pool.rs",
                "./README.md",
                "../outside.rs",
                "src/missing.rs",
                "src/pool.rs",
            ],
        );
        let entry = store.scan().remove(0);
        assert_eq!(
            entry.evidence,
            vec![
                Evidence {
                    path: "src/pool.rs".into(),
                    hash: blob_hash(b"const SIZE: u32 = 8;\n"),
                },
                Evidence {
                    path: "README.md".into(),
                    hash: blob_hash(b"readme\n"),
                },
            ],
            "escaping, missing and repeated paths are dropped"
        );
        let line = std::fs::read_to_string(tmp.path().join("project-memory/decisions.md")).unwrap();
        assert!(line.contains(&format!(
            "[evidence: src/pool.rs@{}, README.md@{}]",
            blob_hash(b"const SIZE: u32 = 8;\n"),
            blob_hash(b"readme\n")
        )));
        assert!(store.changed_evidence(&entry).is_empty());
        assert!(!store.render_index(600).contains("evidence changed"));

        // The code moves: the entry is flagged, everywhere it is read.
        std::fs::write(tmp.path().join("src/pool.rs"), "const SIZE: u32 = 16;\n").unwrap();
        assert_eq!(store.changed_evidence(&entry), vec!["src/pool.rs"]);
        let index = store.render_index(600);
        assert!(index.contains("- [p1] the pool holds eight connections [evidence changed]"));
        assert!(index.contains("re-check before relying on them"));
        let hits = store.search("pool", 10).unwrap();
        assert_eq!(hits[0].evidence_changed, vec!["src/pool.rs"]);

        // Put back, the flag clears; deleted, it returns.
        std::fs::write(tmp.path().join("src/pool.rs"), "const SIZE: u32 = 8;\n").unwrap();
        assert!(store.changed_evidence(&entry).is_empty());
        std::fs::remove_file(tmp.path().join("README.md")).unwrap();
        assert_eq!(store.changed_evidence(&entry), vec!["README.md"]);
    }

    #[test]
    fn reverify_rerecords_the_hash_and_the_flag_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/pool.rs"), "const SIZE: u32 = 8;\n").unwrap();
        add_with_evidence(&store, "the pool holds eight connections", &["src/pool.rs"]);
        // The code moves: flagged on read.
        std::fs::write(tmp.path().join("src/pool.rs"), "const SIZE: u32 = 16;\n").unwrap();
        let entry = store.scan().remove(0);
        assert_eq!(store.changed_evidence(&entry), vec!["src/pool.rs"]);
        // The re-verify (a reread's claim): the hash re-records to the
        // file's current state, the flag clears, and the line carries
        // the new hash.
        let refreshed = store.reverify_evidence("p1").unwrap();
        assert_eq!(refreshed, vec!["src/pool.rs"]);
        let entry = store.scan().remove(0);
        assert!(store.changed_evidence(&entry).is_empty());
        assert_eq!(
            entry.evidence[0].hash,
            blob_hash(b"const SIZE: u32 = 16;\n")
        );
        let line = std::fs::read_to_string(tmp.path().join("project-memory/decisions.md")).unwrap();
        assert!(line.contains(&format!(
            "[evidence: src/pool.rs@{}]",
            blob_hash(b"const SIZE: u32 = 16;\n")
        )));
        // Idempotent: a current entry re-verifies to nothing.
        assert!(store.reverify_evidence("p1").unwrap().is_empty());
        // A vanished cite stays stale (a missing file is not a state
        // to re-record): the flag survives the re-verify, the living
        // cite's hash re-records.
        std::fs::write(tmp.path().join("src/other.rs"), "x\n").unwrap();
        add_with_evidence(&store, "two cites", &["src/pool.rs", "src/other.rs"]);
        std::fs::remove_file(tmp.path().join("src/pool.rs")).unwrap();
        std::fs::write(tmp.path().join("src/other.rs"), "y\n").unwrap();
        let refreshed = store.reverify_evidence("p2").unwrap();
        assert_eq!(refreshed, vec!["src/other.rs"]);
        let entry = store.scan().remove(0);
        assert_eq!(store.changed_evidence(&entry), vec!["src/pool.rs"]);
    }

    #[test]
    fn a_retired_entry_is_not_rechecked_and_a_merge_keeps_both_citations() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        std::fs::write(tmp.path().join("a.rs"), "a\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "b\n").unwrap();
        add_with_evidence(&store, "first", &["a.rs"]);
        add_with_evidence(&store, "second", &["b.rs"]);
        store
            .apply(
                &[Op::Merge {
                    keep: "p1".into(),
                    drop: "p2".into(),
                    text: "both".into(),
                }],
                4096,
            )
            .unwrap();
        let entries = store.scan();
        let paths: Vec<&str> = entries[0]
            .evidence
            .iter()
            .map(|cited| cited.path.as_str())
            .collect();
        assert_eq!(paths, vec!["a.rs", "b.rs"]);
        // p2 is archived: its moved file is history, not a flag.
        std::fs::write(tmp.path().join("b.rs"), "changed\n").unwrap();
        assert!(store.changed_evidence(&entries[1]).is_empty());
        assert_eq!(store.changed_evidence(&entries[0]), vec!["b.rs"]);
    }

    #[test]
    fn a_hand_written_evidence_marker_parses_and_a_bad_one_stays_text() {
        let line = "- [p2] helpful=0 harmful=0 tuned [evidence: src/a.rs@ce013625030b, b.rs@e69de29bb2d1] [source: added: 2026-11-26]";
        let entry = parse_entry(line, Tier::Project, "decisions.md").unwrap();
        assert_eq!(entry.evidence.len(), 2);
        assert_eq!(render_line(&entry), line);
        let odd = "- [p3] helpful=0 harmful=0 see [evidence: just words]";
        let entry = parse_entry(odd, Tier::Project, "decisions.md").unwrap();
        assert!(entry.evidence.is_empty());
        assert_eq!(entry.text, "see [evidence: just words]");
    }

    fn add_from(store: &MemoryStore, text: &str, session: &str) {
        store
            .apply(
                &[Op::Add {
                    file: "decisions.md".into(),
                    text: text.into(),
                    source: Some(session.into()),
                    evidence: Vec::new(),
                }],
                4096,
            )
            .unwrap();
    }

    #[test]
    fn a_correction_flags_what_rested_on_it_one_hop_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        add_from(&store, "the cache is per process", "10"); // p1, corrected
        add_from(&store, "so restarts start cold", "10"); // p2, sibling
        add_from(&store, "warm it at boot", "20"); // p3, builds_on p1
        add_from(&store, "logs go to stderr", "30"); // p4, unrelated
        add_from(&store, "see the cache notes", "40"); // p5, relates_to only
        add_from(&store, "the cache is shared", "50"); // p6, the replacement
        let link = |from: &str, relation: &str| Op::Link {
            from: from.into(),
            to: "p1".into(),
            relation: relation.into(),
        };
        store
            .apply(&[link("p3", "builds_on"), link("p5", "relates_to")], 64)
            .unwrap();
        // p6 rests on p1 as well, but it is the replacement: never flagged.
        store
            .apply(
                &[Op::Link {
                    from: "p6".into(),
                    to: "p1".into(),
                    relation: "solves".into(),
                }],
                16,
            )
            .unwrap();

        let report = store
            .apply(
                &[Op::Supersede {
                    id: "p1".into(),
                    by: Some("p6".into()),
                }],
                16,
            )
            .unwrap();
        assert_eq!(
            report.flagged,
            vec![("p1".to_string(), vec!["p2".to_string(), "p3".to_string()])]
        );
        let entries = store.scan();
        let recheck = |id: &str| {
            entries
                .iter()
                .find(|entry| entry.id == id)
                .unwrap()
                .recheck
                .clone()
        };
        assert_eq!(recheck("p2"), vec!["p1"]);
        assert_eq!(recheck("p3"), vec!["p1"]);
        assert!(recheck("p4").is_empty());
        assert!(recheck("p5").is_empty());
        assert!(recheck("p6").is_empty());
        // Flags change no belief: text and validity are untouched.
        let p3 = entries.iter().find(|entry| entry.id == "p3").unwrap();
        assert_eq!(p3.text, "warm it at boot");
        assert!(p3.valid_until.is_none());
        let line = std::fs::read_to_string(tmp.path().join("project-memory/decisions.md")).unwrap();
        assert!(
            line.contains(
                "warm it at boot [links: builds_on p1] [recheck: p1] [source: 20; added: "
            )
        );
        assert!(line.contains("[recheck: p1]"));

        // The model sees the flag, in the index and on the hit.
        let index = store.render_index(600);
        assert!(index.contains("- [p3] warm it at boot [recheck: p1]"));
        assert!(index.contains("[recheck: id] ones rested on an entry"));
        assert_eq!(store.search("boot", 10).unwrap()[0].recheck, vec!["p1"]);

        // One hop: superseding a flagged entry flags its own dependents
        // (none here), never re-walks p1's.
        let report = store
            .apply(
                &[Op::Supersede {
                    id: "p3".into(),
                    by: None,
                }],
                16,
            )
            .unwrap();
        assert!(report.flagged.is_empty());

        // Keep clears the flag and only the flag.
        store.clear_recheck("p2").unwrap();
        let p2 = store
            .scan()
            .into_iter()
            .find(|entry| entry.id == "p2")
            .unwrap();
        assert!(p2.recheck.is_empty());
        assert_eq!(p2.text, "so restarts start cold");
    }

    #[test]
    fn a_supersede_with_no_dependents_flags_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        add_from(&store, "alone", "1");
        add_from(&store, "elsewhere", "2");
        let report = store
            .apply(
                &[Op::Supersede {
                    id: "p1".into(),
                    by: None,
                }],
                16,
            )
            .unwrap();
        assert!(report.flagged.is_empty());
        assert!(store.scan()[1].recheck.is_empty());
    }

    #[test]
    fn a_recheck_marker_round_trips_and_a_bad_one_stays_text() {
        let line =
            "- [p2] helpful=0 harmful=0 warm it [recheck: p1, g4] [source: 20; added: 2026-11-26]";
        let entry = parse_entry(line, Tier::Project, "decisions.md").unwrap();
        assert_eq!(entry.recheck, vec!["p1", "g4"]);
        assert_eq!(render_line(&entry), line);
        let odd = "- [p3] helpful=0 harmful=0 see [recheck: soon]";
        assert_eq!(
            parse_entry(odd, Tier::Project, "decisions.md")
                .unwrap()
                .text,
            "see [recheck: soon]"
        );
    }

    #[test]
    fn sidecar_only_links_move_into_the_files_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, session) = store_with_roots(&tmp);
        add(&store, "one");
        add(&store, "two");
        // The pre-1.2 state: links in the sidecar, none on the lines,
        // and the migration not yet run.
        session
            .with_connection(|connection| {
                connection.execute_batch(
                    "DELETE FROM memory_migrations;
                     INSERT INTO memory_links VALUES ('p2', 'p1', 'solves');
                     INSERT INTO memory_links VALUES ('p2', 'p404', 'solves');",
                )
            })
            .unwrap();
        store.sync().unwrap();
        let entries = store.scan();
        assert_eq!(
            entries[1].links,
            vec![Link {
                relation: "solves".into(),
                to: "p1".into()
            }]
        );
        assert_eq!(link_count(&session), 1, "the dangling row is dropped");

        // Once: a link removed by hand afterwards stays removed.
        let path = tmp.path().join("project-memory/decisions.md");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace(" [links: solves p1]", "")).unwrap();
        store.sync().unwrap();
        assert!(store.scan()[1].links.is_empty());
    }

    #[test]
    fn a_vanished_file_leaves_every_table() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, session) = store_with_roots(&tmp);
        store
            .apply(
                &[Op::Add {
                    file: "gotchas.md".into(),
                    text: "here today".into(),
                    source: None,
                    evidence: Vec::new(),
                }],
                4096,
            )
            .unwrap();
        assert_eq!(store.search("here today", 10).unwrap().len(), 1);

        std::fs::remove_dir_all(tmp.path().join("project-memory")).unwrap();
        store.sync().unwrap();
        assert!(
            store.search("here today", 10).unwrap().is_empty(),
            "the index must not recall what no longer exists"
        );
        let rows: i64 = session
            .with_connection(|connection| {
                connection.query_row("SELECT count(*) FROM memories", [], |row| row.get(0))
            })
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn the_index_is_budgeted_whole_file_and_never_mid_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        let long = "a fairly long entry line meant to eat budget ".repeat(6);
        store
            .apply(
                &[
                    Op::Add {
                        file: "preferences.md".into(),
                        text: "tabs over spaces".into(),
                        source: None,
                        evidence: Vec::new(),
                    },
                    Op::Add {
                        file: "workflows.md".into(),
                        text: long,
                        source: None,
                        evidence: Vec::new(),
                    },
                ],
                4096,
            )
            .unwrap();
        let index = store.render_index(40);
        assert!(index.contains("tabs over spaces"));
        assert!(
            !index.contains("a fairly long"),
            "the overflowing section drops whole"
        );
        assert!(index.starts_with("# Memory"));
    }

    /// The index is a map (#37): every topic file appears, either with
    /// its strongest entries or as one line naming it, so the model
    /// knows what exists to search for.
    #[test]
    fn the_index_maps_every_file_and_orders_by_votes() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        let root = tmp.path().join("project-memory");
        std::fs::create_dir_all(&root).unwrap();
        let many: String = (1..=30)
            .map(|index| {
                let helpful = if index == 3 { 5 } else { 0 };
                format!(
                    "- [p{index}] helpful={helpful} harmful=0 gotcha number {index} about the build \
                     [source: added: 2026-11-01]\n"
                )
            })
            .collect();
        std::fs::write(root.join("gotchas.md"), format!("# Gotchas\n\n{many}")).unwrap();
        std::fs::write(
            root.join("status-column.md"),
            "# The status column and the riding card\n\n\
             - [p40] helpful=0 harmful=0 the riding card is one line [source: added: 2026-11-01]\n\
             - [p41] helpful=0 harmful=0 TOUCHED folds bash too [source: added: 2026-11-01]\n",
        )
        .unwrap();
        store.sync().unwrap();
        let index = store.render_index(200);
        let gotchas = index.find("## gotchas.md").expect("gotchas partly fits");
        let first = &index[gotchas..].lines().nth(1).unwrap();
        assert!(first.starts_with("- [p3]"), "votes first: {first}");
        assert!(
            index.contains("more in gotchas.md"),
            "the partial file says so:\n{index}"
        );
        assert!(
            index.contains("status-column.md (2 entries): The status column and the riding card"),
            "the file that did not fit is named with its heading:\n{index}"
        );
        assert!(
            index.len() / 4 <= 220,
            "the budget holds: {}",
            index.len() / 4
        );
    }

    /// The lint (#38): every shape that hides a fact from the store or
    /// breaks its structure is a finding with its file, line and fix.
    #[test]
    fn the_lint_names_every_bad_shape_and_passes_a_clean_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(tmp.path().join("real.rs"), "x").unwrap();
        std::fs::write(
            root.join("clean.md"),
            "# Clean\n\n- [p1] helpful=0 harmful=0 a fact [evidence: real.rs@000000000000] [source: added: 2026-11-01]\n\
             - [p8] helpful=0 harmful=0 the grammar is `- [pN] helpful=N harmful=M text` [source: added: 2026-11-01]\n\
             - [p2] helpful=0 harmful=0 another [links: builds_on p1] [source: added: 2026-11-01]\n",
        )
        .unwrap();
        assert!(lint_tier(&root, Tier::Project, Some(tmp.path())).is_empty());

        std::fs::write(
            root.join("bad.md"),
            "# Bad\n\
             - [p3] the counters are missing\n\
             - [p4] helpful=0 harmful=0 - [v1] helpful=0 harmful=0 wrapped twice [source: added: 2026-11-01]\n\
             - [p5] helpful=0 harmful=0 # A heading [source: added: 2026-11-01]\n\
             some prose the index cannot see\n\
             - [p1] helpful=0 harmful=0 a duplicate id [source: added: 2026-11-01]\n\
             - [p6] helpful=0 harmful=0 dangling [links: builds_on p99] [recheck: p98] [source: added: 2026-11-01] [superseded by: p97; valid until: 2026-11-02]\n\
             - [p7] helpful=0 harmful=0 cites a ghost [evidence: gone.rs@000000000000] [source: added: 2026-11-01]\n",
        )
        .unwrap();
        let findings = lint_tier(&root, Tier::Project, Some(tmp.path()));
        let said = |needle: &str| {
            findings
                .iter()
                .any(|finding| finding.message.contains(needle))
        };
        for needle in [
            "does not parse",
            "wrapped twice",
            "heading stored as an entry",
            "prose",
            "duplicate id p1",
            "p99",
            "p98",
            "p97",
            "gone.rs",
        ] {
            assert!(said(needle), "missing {needle:?} in {findings:#?}");
        }
        assert!(
            findings
                .iter()
                .all(|finding| finding.file == "bad.md" || finding.message.contains("p1"))
        );
        assert!(findings.iter().all(|finding| finding.line > 0));
    }

    /// Models search in natural language: other word forms, extra
    /// words, punctuation. The eval caught every query missing seeded
    /// entries, because MATCH took the raw text: all words required,
    /// no stemming, punctuation parsed as FTS syntax.
    /// #44: a correction starts one vote up; anything else at zero.
    #[test]
    fn a_correction_starts_one_vote_up() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(
            tmp.path().join("project-memory"),
            tmp.path().join("global"),
            std::sync::Arc::new(crate::SessionStore::open(Path::new(":memory:")).unwrap()),
        );
        let add = |text: &str, source: Option<&str>| Op::Add {
            file: "conventions.md".into(),
            text: text.into(),
            source: source.map(str::to_string),
            evidence: Vec::new(),
        };
        store
            .apply(
                &[
                    add(
                        "use cargo clean, never rm -rf target",
                        Some(CORRECTION_SOURCE),
                    ),
                    add("the build takes two minutes", Some("41")),
                ],
                4096,
            )
            .unwrap();
        let entries = store.scan();
        let votes = |needle: &str| {
            entries
                .iter()
                .find(|entry| entry.text.contains(needle))
                .map(|entry| (entry.helpful, entry.source_session.clone()))
                .unwrap()
        };
        assert_eq!(votes("cargo clean"), (1, Some("correction".into())));
        assert_eq!(votes("two minutes").0, 0);
    }

    /// The writer emits `[source: added: D]` for an entry with no
    /// session; the parser used to file the date as the source, so
    /// every such entry searched as "added -".
    #[test]
    fn a_sourceless_entry_keeps_its_date_through_a_round_trip() {
        let line = "- [p1] helpful=0 harmful=0 a fact [source: added: 2026-11-01]";
        let entry = parse_entry(line, Tier::Project, "x.md").unwrap();
        assert_eq!(entry.added_at.as_deref(), Some("2026-11-01"));
        assert_eq!(entry.source_session, None);
        assert_eq!(render_line(&entry), line);
        let both = parse_entry(
            "- [p2] helpful=0 harmful=0 b [source: 34; added: 2026-11-02]",
            Tier::Project,
            "x.md",
        )
        .unwrap();
        assert_eq!(both.source_session.as_deref(), Some("34"));
        assert_eq!(both.added_at.as_deref(), Some("2026-11-02"));
    }

    #[test]
    fn natural_queries_find_entries_in_other_word_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project-memory");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("decisions.md"),
            "- [p1] helpful=0 harmful=0 release artifacts are signed with the maintainer's local GPG key [source: added: 2026-11-01]\n\
             - [p2] helpful=0 harmful=0 POOL_SIZE in src/pool.rs must stay 8: a larger pool deadlocks the teardown [source: added: 2026-11-01]\n\
             - [p3] helpful=0 harmful=0 the docs site builds with mdbook [source: added: 2026-11-01]\n",
        )
        .unwrap();
        let store = MemoryStore::open(
            project,
            tmp.path().join("global"),
            std::sync::Arc::new(crate::SessionStore::open(Path::new(":memory:")).unwrap()),
        );
        store.sync().unwrap();
        let first = |query: &str| {
            store
                .search(query, 10)
                .unwrap_or_else(|error| panic!("{query:?}: {error}"))
                .first()
                .map(|hit| hit.id.clone())
        };
        assert_eq!(first("release signing GPG key").as_deref(), Some("p1"));
        assert_eq!(first("how are releases signed?").as_deref(), Some("p1"));
        assert_eq!(first("POOL_SIZE throughput limit").as_deref(), Some("p2"));
        assert_eq!(first("pool size: deadlock (CI)").as_deref(), Some("p2"));
        assert_eq!(first("src/pool.rs").as_deref(), Some("p2"));
        // Syntax and noise never error, and stopwords alone match nothing.
        for query in [
            "AND OR NOT",
            "\"unbalanced",
            "a-b*c^d",
            "the of and",
            "",
            "NEAR(x y)",
        ] {
            store
                .search(query, 10)
                .unwrap_or_else(|error| panic!("{query:?}: {error}"));
        }
        assert!(store.search("the of and", 10).unwrap().is_empty());
    }

    #[test]
    fn a_custom_topic_file_still_reaches_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        store
            .apply(
                &[Op::Add {
                    file: "project-layout.md".into(),
                    text: "the tests live beside the code".into(),
                    source: None,
                    evidence: Vec::new(),
                }],
                4096,
            )
            .unwrap();
        assert!(store.render_index(4096).contains("project-layout.md"));
    }

    #[test]
    fn merge_consolidates_duplicates_and_keeps_both_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        store
            .apply(
                &[
                    Op::Add {
                        file: "gotchas.md".into(),
                        text: "cite the flake before blaming the test".into(),
                        source: Some("388".into()),
                        evidence: Vec::new(),
                    },
                    Op::Add {
                        file: "gotchas.md".into(),
                        text: "cite the flake before you blame the test".into(),
                        source: Some("401".into()),
                        evidence: Vec::new(),
                    },
                ],
                10_000,
            )
            .unwrap();
        let entries = store.scan();
        let mut ids: Vec<String> = entries.iter().map(|entry| entry.id.clone()).collect();
        ids.sort();
        assert_eq!(ids.len(), 2, "both entries exist pre-merge");
        // Merge the second into the first (first id survives).
        let report = store
            .apply(
                &[Op::Merge {
                    keep: ids[0].clone(),
                    drop: ids[1].clone(),
                    text: "cite the flake before blaming the test".into(),
                }],
                10_000,
            )
            .unwrap();
        assert_eq!(report.applied.len(), 1, "merge applied: {:?}", report);
        let after = store.scan();
        let live: Vec<_> = after.iter().filter(|e| e.valid_until.is_none()).collect();
        assert_eq!(live.len(), 1, "one live entry post-merge");
        assert_eq!(live[0].id, ids[0], "the survivor keeps its id");
        let sources = live[0].source_session.as_deref().unwrap_or("");
        assert!(
            sources.contains("388"),
            "survivor's source stays: {sources}"
        );
        assert!(sources.contains("401"), "loser's source joins: {sources}");
        // The loser is archived, not deleted: its line remains in the file.
        let text = std::fs::read_to_string(store.root(Tier::Project).join("gotchas.md")).unwrap();
        assert!(text.contains(&format!("- [{}]", ids[1])));
        assert!(text.contains("[archived:"));
    }

    #[test]
    fn cross_tier_merges_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        add_global(
            &store,
            "preferences.md",
            "cite the flake first",
            Some("388".into()),
        )
        .unwrap();
        store
            .apply(
                &[Op::Add {
                    file: "gotchas.md".into(),
                    text: "cite the flake first".into(),
                    source: Some("401".into()),
                    evidence: Vec::new(),
                }],
                10_000,
            )
            .unwrap();
        let entries = store.scan();
        let global = entries
            .iter()
            .find(|entry| entry.id.starts_with('g'))
            .unwrap();
        let project = entries
            .iter()
            .find(|entry| entry.id.starts_with('p'))
            .unwrap();
        let report = store
            .apply(
                &[Op::Merge {
                    keep: global.id.clone(),
                    drop: project.id.clone(),
                    text: "one text".into(),
                }],
                10_000,
            )
            .unwrap();
        assert!(report.applied.is_empty(), "cross-tier merge is skipped");
    }

    #[test]
    fn global_entries_get_their_own_sequence_and_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        store
            .apply(
                &[Op::Add {
                    file: "conventions.md".into(),
                    text: "project fact".into(),
                    source: None,
                    evidence: Vec::new(),
                }],
                4096,
            )
            .unwrap();
        add_global(&store, "preferences.md", "user prefers vim bindings", None).unwrap();
        let entries = store.scan();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "p1");
        assert_eq!(entries[1].id, "g1");
        // The global root holds it, the project root does not.
        assert!(tmp.path().join("global-memory/preferences.md").exists());
        assert!(!tmp.path().join("project-memory/preferences.md").exists());
    }

    #[test]
    fn hostile_file_names_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _session) = store_with_roots(&tmp);
        for hostile in ["../escape.md", "MEMORY.md", "", "no-slash/inside.md"] {
            let report = store
                .apply(
                    &[Op::Add {
                        file: hostile.into(),
                        text: "x".into(),
                        source: None,
                        evidence: Vec::new(),
                    }],
                    4096,
                )
                .unwrap();
            assert_eq!(report.applied.len(), 0, "{hostile} must never be applied");
            assert_eq!(
                report.skipped.len(),
                1,
                "{hostile} must be named in the report"
            );
        }
        assert!(!tmp.path().join("escape.md").exists());
    }
}
