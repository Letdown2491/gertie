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
    pub tier: Tier,
    pub file: String,
    pub text: String,
    pub added_at: Option<String>,
    /// The hit's typed links, so the model can follow them by id.
    pub links: Vec<Link>,
    /// Cited files that changed (or vanished) since the entry was
    /// written. Non-empty means: re-check before relying on it.
    pub evidence_changed: Vec<String>,
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
            for known_path in known.keys() {
                let still_there = on_disk
                    .iter()
                    .any(|(_, path, _, _)| path.display().to_string() == *known_path);
                if still_there {
                    continue;
                }
                let tier = if known_path.contains("/.local/share/") {
                    Tier::Global
                } else {
                    Tier::Project
                };
                connection.execute(
                    "DELETE FROM memories_fts WHERE id IN
                         (SELECT id FROM memories WHERE tier = ?1 AND file = ?2)",
                    rusqlite::params![tier.name(), file_name(Path::new(known_path))],
                )?;
                connection.execute(
                    "DELETE FROM memories WHERE tier = ?1 AND file = ?2",
                    rusqlite::params![tier.name(), file_name(Path::new(known_path))],
                )?;
                connection.execute("DELETE FROM memory_files WHERE path = ?1", [known_path])?;
            }

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
        let query = sanitize_untrusted(query, 256);
        self.store
            .with_connection(|connection| {
                let mut statement = connection.prepare(
                    "SELECT memories.id, memories.tier, memories.file, memories.content,
                        memories.added_at
                 FROM memories_fts
                 JOIN memories ON memories.id = memories_fts.id
                 WHERE memories_fts MATCH ?1 AND memories.valid_until IS NULL
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
                        links: Vec::new(),
                        evidence_changed: Vec::new(),
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
                })
            })?;
            rows.collect()
        })
    }

    /// Applies delta ops, budgeted. Each op is one line-level edit; the
    /// budget caps the total text this call may add or rewrite, so a
    /// runaway extraction cannot balloon the store in one pass.
    pub fn apply(&self, ops: &[Op], budget_tokens: usize) -> rusqlite::Result<ApplyReport> {
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
                    self.supersede_entry(id, by.as_deref())
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
        let line = render_line(&MemoryEntry {
            id: id.clone(),
            file: file.clone(),
            text: text.clone(),
            helpful: 0,
            harmful: 0,
            source_session: source,
            added_at: Some(date),
            valid_until: None,
            superseded_by: None,
            links: Vec::new(),
            evidence: self.evidence_for(evidence),
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
        let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut any_changed = false;
        for entry in self.scan() {
            if entry.valid_until.is_some() {
                continue;
            }
            let mark = if self.changed_evidence(&entry).is_empty() {
                ""
            } else {
                any_changed = true;
                " [evidence changed]"
            };
            sections
                .entry(entry.file.clone())
                .or_default()
                .push(format!("- [{}] {}{mark}", entry.id, entry.text));
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
        let mut index = String::from("# Memory\n");
        let mut spent = 3usize; // the header
        if any_changed {
            // One line, only when it applies: the mark's meaning.
            index.push_str(
                "\nEntries marked [evidence changed] cite files that changed since; \
                 re-check before relying on them.\n",
            );
            spent += 20;
        }
        for file in order {
            let lines = &sections[file.as_str()];
            let cost = lines.iter().map(|line| line.len() / 4 + 1).sum::<usize>() + 1;
            if spent + cost > budget_tokens {
                continue;
            }
            spent += cost;
            index.push_str(&format!("\n## {}\n", file));
            for line in lines {
                index.push_str(line);
                index.push('\n');
            }
        }
        index
    }
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
        } else if let Some(date) = inner.strip_prefix("archived:") {
            valid_until = Some(date.trim().to_string());
        } else if inner.starts_with("source:") || inner.starts_with("added:") {
            for part in inner.split(';') {
                let part = part.trim();
                if let Some(source) = part.strip_prefix("source:") {
                    source_session = Some(source.trim().to_string());
                } else if let Some(added) = part.strip_prefix("added:") {
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
    })
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
