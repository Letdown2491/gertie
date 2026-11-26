//! The repo map (#41): the workspace's source files and their top-level
//! symbols, parsed with tree-sitter, ranked for the session and cut to a
//! budget.
//!
//! Models spend rounds finding code: list a directory, read the wrong
//! file, grep, read again. A few hundred tokens naming files and what
//! they define send the model to the right file first. Two surfaces
//! share one [`RepoMap`] per workspace root (and so one cache):
//!
//! - the session framing ([`RepoMapSource`]), ranked by the session's
//!   hints and frozen by the loop, so it costs the cache nothing;
//! - the `repo_map` tool, for a directory's files or one file's
//!   signatures with line numbers, current at the time of the call.
//!
//! Parsing is incremental: each file's outline is cached under its git
//! blob hash (the hash memory evidence uses), so a changed file is
//! re-parsed and an unchanged one never is. The cost is bounded: the
//! walk stops at [`MAX_FILES`], files over [`MAX_FILE_BYTES`] are listed
//! but not parsed, and the framing parses only the files it shows.

use gertie_core::repo_map::{MapHints, RepoMapSource};
use gertie_core::{Tool, ToolResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Source files walked before the walk stops: a monorepo's map is its
/// summary line, not a minute of parsing.
pub const MAX_FILES: usize = 20_000;

/// Files larger than this are listed, never parsed (generated code,
/// vendored bundles).
pub const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Files the framing looks at, at most: it shows a few dozen.
const FRAMING_PARSES: usize = 400;

/// The tool's answer cap, in characters.
const TOOL_CHARS: usize = 8_000;

/// One file line's cap in the framing, in characters.
const LINE_CHARS: usize = 220;

/// Directories never walked: build output, dependencies, VCS, caches.
const SKIPPED_DIRS: [&str; 9] = [
    ".git",
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    ".pixi",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    Rust,
    Python,
    Go,
    JavaScript,
    TypeScript,
    Tsx,
}

impl Lang {
    fn of(path: &str) -> Option<Self> {
        let extension = path.rsplit_once('.')?.1;
        Some(match extension {
            "rs" => Self::Rust,
            "py" => Self::Python,
            "go" => Self::Go,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            _ => return None,
        })
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

/// One top-level definition: a short label for the framing ("fn new",
/// "impl Drop for Pool"), its signature for the tool, its 1-based line,
/// and its members (an impl's or class's methods).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub label: String,
    pub signature: String,
    pub line: usize,
    pub members: Vec<Symbol>,
}

/// A file's outline under the blob hash it was parsed from.
type Cached = (String, Arc<Vec<Symbol>>);

/// One workspace's map and its outline cache.
pub struct RepoMap {
    root: PathBuf,
    /// Relative path to (blob hash, outline).
    cache: Mutex<HashMap<String, Cached>>,
}

/// One map per root, process-wide: the tool and the framing share the
/// cache, and a re-rooted registry finds the map it had.
static MAPS: Mutex<Option<HashMap<PathBuf, Arc<RepoMap>>>> = Mutex::new(None);

impl RepoMap {
    /// The map for `root`, created on first use.
    pub fn for_root(root: &Path) -> Arc<Self> {
        let mut maps = MAPS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        maps.get_or_insert_with(HashMap::new)
            .entry(root.to_path_buf())
            .or_insert_with(|| {
                Arc::new(Self {
                    root: root.to_path_buf(),
                    cache: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    /// The workspace's source files, relative, sorted; true when the
    /// walk stopped at [`MAX_FILES`].
    pub fn files(&self) -> (Vec<String>, bool) {
        self.files_under(&self.root)
    }

    fn files_under(&self, base: &Path) -> (Vec<String>, bool) {
        let mut files = Vec::new();
        let mut capped = false;
        let walk = walkdir::WalkDir::new(base)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0 || {
                    let name = entry.file_name().to_string_lossy();
                    !name.starts_with('.')
                        && !(entry.file_type().is_dir() && SKIPPED_DIRS.contains(&name.as_ref()))
                }
            });
        for entry in walk.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&self.root) else {
                continue;
            };
            let relative = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if Lang::of(&relative).is_none() {
                continue;
            }
            if files.len() == MAX_FILES {
                capped = true;
                break;
            }
            files.push(relative);
        }
        files.sort();
        (files, capped)
    }

    /// A file's outline, from the cache when its blob hash is unchanged.
    /// None: not a source file, unreadable, or too large to parse.
    pub fn symbols(&self, relative: &str) -> Option<Arc<Vec<Symbol>>> {
        let lang = Lang::of(relative)?;
        let path = self.root.join(relative);
        if std::fs::metadata(&path).ok()?.len() > MAX_FILE_BYTES {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let hash = gertie_core::blob_hash(&bytes);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached, symbols)) = cache.get(relative)
            && *cached == hash
        {
            return Some(symbols.clone());
        }
        let source = String::from_utf8(bytes).ok()?;
        let symbols = Arc::new(outline(lang, &source)?);
        cache.insert(relative.to_string(), (hash, symbols.clone()));
        Some(symbols)
    }

    /// Files ranked for the session: named by the user, touched, cited
    /// by memory, a name the user's words contain, an entry point; then
    /// shallow before deep, then by path. Every sort key is the file's
    /// own, so the order is deterministic for given hints.
    fn ranked(&self, files: Vec<String>, hints: &MapHints) -> Vec<String> {
        let said = hints.said.to_lowercase();
        let words: Vec<&str> = said
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|word| word.len() >= 4)
            .collect();
        let score = |path: &str| -> u32 {
            let name = path.rsplit('/').next().unwrap_or(path);
            let stem = name.split('.').next().unwrap_or(name).to_lowercase();
            let mut score = 0;
            if said.contains(&path.to_lowercase()) || said.contains(&name.to_lowercase()) {
                score += 100;
            }
            if hints.touched.iter().any(|touched| touched == path) {
                score += 80;
            }
            if hints.cited.iter().any(|cited| cited == path) {
                score += 60;
            }
            if stem.len() >= 4
                && words
                    .iter()
                    .any(|word| *word == stem || stem.split('_').any(|part| part == *word))
            {
                score += 40;
            }
            if matches!(
                name,
                "lib.rs"
                    | "main.rs"
                    | "main.go"
                    | "main.py"
                    | "__init__.py"
                    | "index.ts"
                    | "index.js"
                    | "app.py"
            ) {
                score += 10;
            }
            score
        };
        let mut ranked: Vec<(u32, usize, String)> = files
            .into_iter()
            .map(|path| (score(&path), path.matches('/').count(), path))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        ranked.into_iter().map(|(_, _, path)| path).collect()
    }

    /// The framing body: ranked file lines while they fit the budget,
    /// then one summary line for the rest. Never longer than
    /// `budget_tokens * 4` characters; a budget too small for any file
    /// line leaves only the summary.
    pub fn framing(&self, hints: &MapHints, budget_tokens: usize) -> Option<String> {
        let (files, capped) = self.files();
        if files.is_empty() {
            return None;
        }
        let budget = budget_tokens * 4;
        let total = files.len();
        let ranked = self.ranked(files, hints);
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0;
        let mut shown: Vec<&str> = Vec::new();
        // Room kept for the summary line, which must always fit.
        let reserve = 160.min(budget);
        // Parses spent on the framing: on a huge repo the lines run out
        // of budget long before the files run out.
        for (tried, path) in ranked.iter().enumerate() {
            if budget.saturating_sub(used + reserve) < 40 || tried == FRAMING_PARSES {
                break;
            }
            let Some(symbols) = self.symbols(path) else {
                continue;
            };
            if symbols.is_empty() {
                continue;
            }
            let line = file_line(path, &symbols, LINE_CHARS);
            if used + line.len() + 1 + reserve > budget {
                // A long line may not fit where a short one would: keep
                // looking, the budget is the rule, not the order.
                continue;
            }
            used += line.len() + 1;
            lines.push(line);
            shown.push(path);
        }
        let rest: Vec<&String> = ranked
            .iter()
            .filter(|path| !shown.contains(&path.as_str()))
            .collect();
        if !rest.is_empty() || capped {
            let summary = summary_line(&rest, capped, total);
            let room = budget.saturating_sub(used);
            lines.push(truncate_chars(&summary, room));
        }
        let body = lines.join("\n");
        (!body.trim().is_empty()).then_some(body)
    }

    /// The tool's answer for `path`: the whole workspace's directories,
    /// one directory's files with their symbols, or one file's
    /// signatures with line numbers.
    pub fn expand(&self, path: Option<&str>) -> Result<String, String> {
        let path = path
            .map(|path| path.trim().trim_start_matches("./").trim_end_matches('/'))
            .filter(|path| !path.is_empty() && *path != ".");
        let Some(path) = path else {
            return Ok(self.overview());
        };
        let full = crate::resolve(&self.root, path)?;
        if full.is_file() {
            if Lang::of(path).is_none() {
                return Err(format!(
                    "repo_map: {path} is not a source file it can outline (Rust, Python, Go, \
                     JavaScript, TypeScript); read_file it"
                ));
            }
            let Some(symbols) = self.symbols(path) else {
                return Err(format!(
                    "repo_map: {path} could not be parsed (unreadable, or over {} KB); \
                     read_file it",
                    MAX_FILE_BYTES / 1024
                ));
            };
            if symbols.is_empty() {
                return Ok(format!("{path}: no top-level definitions"));
            }
            let mut out = format!("{path}\n");
            for symbol in symbols.iter() {
                out.push_str(&format!("L{} {}\n", symbol.line, symbol.signature));
                for member in &symbol.members {
                    out.push_str(&format!("  L{} {}\n", member.line, member.signature));
                }
            }
            return Ok(cap(out));
        }
        if !full.is_dir() {
            return Err(format!("repo_map: {path} does not exist"));
        }
        let (files, capped) = self.files_under(&full);
        if files.is_empty() {
            return Ok(format!("{path}/: no source files"));
        }
        let mut out = String::new();
        let mut listed = 0;
        for file in &files {
            let line = match self.symbols(file) {
                Some(symbols) if !symbols.is_empty() => file_line(file, &symbols, LINE_CHARS),
                _ => file.clone(),
            };
            if out.len() + line.len() + 80 > TOOL_CHARS {
                break;
            }
            out.push_str(&line);
            out.push('\n');
            listed += 1;
        }
        if listed < files.len() || capped {
            out.push_str(&format!(
                "… {} more files; repo_map a subdirectory to see them\n",
                files.len() - listed
            ));
        }
        Ok(out)
    }

    /// Top-level directories with their source file counts, then the
    /// root's own files.
    fn overview(&self) -> String {
        let (files, capped) = self.files();
        if files.is_empty() {
            return "no source files in the workspace".into();
        }
        let mut directories: std::collections::BTreeMap<String, usize> = Default::default();
        let mut top: Vec<&str> = Vec::new();
        for file in &files {
            match file.split_once('/') {
                Some((directory, _)) => {
                    *directories.entry(format!("{directory}/")).or_default() += 1
                }
                None => top.push(file),
            }
        }
        let mut out = format!(
            "{} source files{}\n",
            files.len(),
            if capped {
                " (the walk stopped there)"
            } else {
                ""
            }
        );
        for (directory, count) in directories {
            out.push_str(&format!("{directory} {count} files\n"));
        }
        for file in top {
            out.push_str(&format!("{file}\n"));
        }
        out.push_str(
            "repo_map a directory for its files and symbols, or a file for its signatures\n",
        );
        cap(out)
    }
}

impl RepoMapSource for RepoMap {
    fn render(&self, hints: &MapHints, budget_tokens: usize) -> Option<String> {
        self.framing(hints, budget_tokens)
    }
}

/// "src/pool.rs: struct Pool; fn new; impl Drop for Pool", cut to
/// `limit` characters with a count of what was cut.
fn file_line(path: &str, symbols: &[Symbol], limit: usize) -> String {
    // Public items first when the line has to be cut: they are what
    // other files call, and what a task names.
    let private = |symbol: &Symbol| match Lang::of(path) {
        Some(Lang::Rust) => {
            !symbol.signature.starts_with("pub") && !symbol.label.starts_with("impl")
        }
        Some(Lang::Python) => symbol
            .label
            .rsplit(' ')
            .next()
            .is_some_and(|name| name.starts_with('_')),
        _ => false,
    };
    let mut ordered: Vec<&Symbol> = symbols.iter().collect();
    ordered.sort_by_key(|symbol| private(symbol));
    let symbols = ordered;
    let mut line = format!("{path}:");
    for (index, symbol) in symbols.iter().enumerate() {
        let separator = if index == 0 { " " } else { "; " };
        let more = format!("; +{} more", symbols.len() - index);
        if line.len() + separator.len() + symbol.label.len() + more.len() > limit {
            line.push_str(if index == 0 { " " } else { "; " });
            line.push_str(&format!("+{} more", symbols.len() - index));
            break;
        }
        line.push_str(separator);
        line.push_str(&symbol.label);
    }
    line
}

/// "… 214 more source files (src/ 120, tests/ 40, …): repo_map lists them".
fn summary_line(rest: &[&String], capped: bool, total: usize) -> String {
    let mut directories: std::collections::BTreeMap<&str, usize> = Default::default();
    for path in rest {
        let directory = path.split_once('/').map_or(".", |(directory, _)| directory);
        *directories.entry(directory).or_default() += 1;
    }
    let mut by_count: Vec<(&str, usize)> = directories.into_iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let breakdown: Vec<String> = by_count
        .iter()
        .take(4)
        .map(|(directory, count)| {
            if *directory == "." {
                format!("top level {count}")
            } else {
                format!("{directory}/ {count}")
            }
        })
        .collect();
    let more = if by_count.len() > 4 { ", …" } else { "" };
    let walked = if capped {
        format!(" (the walk stopped at {total} files)")
    } else {
        String::new()
    };
    format!(
        "… {} more source files{walked} ({}{more}): repo_map lists a directory",
        rest.len(),
        breakdown.join(", ")
    )
}

/// `text` cut to at most `limit` bytes, an ellipsis marking the cut.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let ellipsis = '…'.len_utf8();
    if limit < ellipsis {
        return String::new();
    }
    let mut end = limit - ellipsis;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn cap(text: String) -> String {
    if text.len() <= TOOL_CHARS {
        return text;
    }
    let mut end = TOOL_CHARS;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [cut at {TOOL_CHARS} characters]", &text[..end])
}

/// A file's top-level definitions. None when the grammar fails to load
/// (it cannot, for the built-in languages) or the parse yields nothing.
fn outline(lang: Lang, source: &str) -> Option<Vec<Symbol>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang.grammar()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let mut symbols = Vec::new();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        symbols.extend(symbols_of(lang, node, source));
    }
    Some(symbols)
}

fn text<'a>(node: tree_sitter::Node, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn field<'a>(node: tree_sitter::Node, name: &str, source: &'a str) -> Option<&'a str> {
    node.child_by_field_name(name)
        .map(|child| text(child, source))
}

/// The definition's header: its text up to the body, whitespace
/// collapsed, cut to a line's worth.
fn signature(node: tree_sitter::Node, source: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or_else(|| {
            let full = text(node, source);
            node.start_byte() + full.find(['{', '\n']).unwrap_or(full.len())
        });
    let header = &source[node.start_byte()..end.max(node.start_byte())];
    let collapsed = header.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_end_matches([':', '{', ' ', '=']).to_string();
    truncate_chars(&trimmed, 200)
}

fn symbol(label: String, node: tree_sitter::Node, source: &str) -> Symbol {
    Symbol {
        label,
        signature: signature(node, source),
        line: node.start_position().row + 1,
        members: Vec::new(),
    }
}

/// Members of a body node: the children `keep` accepts, as symbols.
fn members(lang: Lang, body: Option<tree_sitter::Node>, source: &str) -> Vec<Symbol> {
    let Some(body) = body else {
        return Vec::new();
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .flat_map(|child| match (lang, child.kind()) {
            (Lang::Rust, "function_item" | "function_signature_item") => {
                field(child, "name", source).map(|name| symbol(format!("fn {name}"), child, source))
            }
            (Lang::Python, "function_definition") => field(child, "name", source)
                .map(|name| symbol(format!("def {name}"), child, source)),
            (Lang::Python, "decorated_definition") => child
                .child_by_field_name("definition")
                .and_then(|definition| {
                    field(definition, "name", source)
                        .map(|name| symbol(format!("def {name}"), definition, source))
                }),
            (
                Lang::JavaScript | Lang::TypeScript | Lang::Tsx,
                "method_definition" | "method_signature" | "abstract_method_signature",
            ) => field(child, "name", source).map(|name| symbol(name.to_string(), child, source)),
            _ => None,
        })
        .collect()
}

fn symbols_of(lang: Lang, node: tree_sitter::Node, source: &str) -> Vec<Symbol> {
    let named = |prefix: &str| {
        field(node, "name", source)
            .map(|name| vec![symbol(format!("{prefix} {name}"), node, source)])
            .unwrap_or_default()
    };
    match lang {
        Lang::Rust => match node.kind() {
            "function_item" => named("fn"),
            "struct_item" => named("struct"),
            "enum_item" => named("enum"),
            "union_item" => named("union"),
            "type_item" => named("type"),
            "const_item" => named("const"),
            "static_item" => named("static"),
            // An inline module is a definition; `mod x;` only points at
            // a file the map lists anyway, and `mod tests` is not where
            // anything a task names lives.
            "mod_item" => match (
                field(node, "name", source),
                node.child_by_field_name("body"),
            ) {
                (Some("tests"), _) | (_, None) => Vec::new(),
                _ => named("mod"),
            },
            "macro_definition" => field(node, "name", source)
                .map(|name| vec![symbol(format!("macro {name}!"), node, source)])
                .unwrap_or_default(),
            "trait_item" => {
                let mut symbols = named("trait");
                if let Some(first) = symbols.first_mut() {
                    first.members = members(lang, node.child_by_field_name("body"), source);
                }
                symbols
            }
            "impl_item" => {
                let strip = |name: &str| name.split('<').next().unwrap_or(name).trim().to_string();
                let Some(target) = field(node, "type", source).map(strip) else {
                    return Vec::new();
                };
                let label = match field(node, "trait", source).map(strip) {
                    Some(implemented) => format!("impl {implemented} for {target}"),
                    None => format!("impl {target}"),
                };
                let mut item = symbol(label, node, source);
                item.members = members(lang, node.child_by_field_name("body"), source);
                vec![item]
            }
            _ => Vec::new(),
        },
        Lang::Python => match node.kind() {
            "function_definition" => named("def"),
            "class_definition" => {
                let mut symbols = named("class");
                if let Some(first) = symbols.first_mut() {
                    first.members = members(lang, node.child_by_field_name("body"), source);
                }
                symbols
            }
            "decorated_definition" => node
                .child_by_field_name("definition")
                .map(|definition| symbols_of(lang, definition, source))
                .unwrap_or_default(),
            // Module constants, the UPPER_CASE convention: what a task
            // names ("the retry cap") is often one of these.
            "expression_statement" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .filter(|child| child.kind() == "assignment")
                    .filter_map(|assignment| {
                        let name = field(assignment, "left", source)?;
                        let constant = name.len() > 1
                            && name
                                .chars()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                        constant.then(|| Symbol {
                            label: name.to_string(),
                            signature: truncate_chars(
                                &text(assignment, source)
                                    .split_whitespace()
                                    .collect::<Vec<_>>()
                                    .join(" "),
                                200,
                            ),
                            line: assignment.start_position().row + 1,
                            members: Vec::new(),
                        })
                    })
                    .collect()
            }
            _ => Vec::new(),
        },
        Lang::Go => match node.kind() {
            "function_declaration" => named("func"),
            "method_declaration" => {
                let receiver = node
                    .child_by_field_name("receiver")
                    .map(|receiver| {
                        text(receiver, source)
                            .trim_matches(['(', ')'])
                            .split_whitespace()
                            .last()
                            .unwrap_or("")
                            .trim_start_matches('*')
                            .to_string()
                    })
                    .unwrap_or_default();
                field(node, "name", source)
                    .map(|name| vec![symbol(format!("func {receiver}.{name}"), node, source)])
                    .unwrap_or_default()
            }
            "type_declaration" | "const_declaration" | "var_declaration" => {
                let prefix = match node.kind() {
                    "type_declaration" => "type",
                    "const_declaration" => "const",
                    _ => "var",
                };
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .filter(|child| {
                        matches!(
                            child.kind(),
                            "type_spec" | "type_alias" | "const_spec" | "var_spec"
                        )
                    })
                    .filter_map(|spec| {
                        let name = spec
                            .child_by_field_name("name")
                            .map(|name| text(name, source).to_string())?;
                        Some(symbol(format!("{prefix} {name}"), spec, source))
                    })
                    .collect()
            }
            _ => Vec::new(),
        },
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => match node.kind() {
            "function_declaration" | "generator_function_declaration" => named("function"),
            "class_declaration" | "abstract_class_declaration" => {
                let mut symbols = named("class");
                if let Some(first) = symbols.first_mut() {
                    first.members = members(lang, node.child_by_field_name("body"), source);
                }
                symbols
            }
            "interface_declaration" => named("interface"),
            "type_alias_declaration" => named("type"),
            "enum_declaration" => named("enum"),
            "lexical_declaration" | "variable_declaration" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .filter(|child| child.kind() == "variable_declarator")
                    .filter_map(|declarator| {
                        let name = field(declarator, "name", source)?;
                        Some(Symbol {
                            label: format!("const {name}"),
                            signature: truncate_chars(
                                text(declarator, source).lines().next().unwrap_or(""),
                                200,
                            ),
                            line: declarator.start_position().row + 1,
                            members: Vec::new(),
                        })
                    })
                    .collect()
            }
            "export_statement" => node
                .child_by_field_name("declaration")
                .map(|declaration| symbols_of(lang, declaration, source))
                .unwrap_or_default(),
            _ => Vec::new(),
        },
    }
}

/// The `repo_map` tool: the same map, on demand.
pub struct RepoMapTool {
    map: Arc<RepoMap>,
}

impl RepoMapTool {
    pub fn new(root: &Path) -> Self {
        Self {
            map: RepoMap::for_root(root),
        }
    }
}

#[derive(serde::Deserialize)]
struct RepoMapArgs {
    #[serde(default)]
    path: Option<String>,
}

impl Tool for RepoMapTool {
    fn name(&self) -> &'static str {
        "repo_map"
    }

    fn description(&self) -> String {
        "List the workspace's code structure. With no path: the directories and their source \
         file counts. With a directory: its source files and each file's top-level symbols. \
         With a file: every top-level signature, and an impl's or class's methods, with line \
         numbers to read_file at. Use it to find where something is defined before opening \
         files. Covers Rust, Python, Go, JavaScript and TypeScript."
            .into()
    }

    fn terse_description(&self) -> String {
        "List code structure: directories, a directory's files and symbols, or a file's \
         signatures with line numbers."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "a directory or source file, relative to the workspace root; omit for the overview"
                }
            }
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
        let map = self.map.clone();
        let arguments = arguments.to_string();
        crate::blocking(move || {
            let args: RepoMapArgs = crate::parse(&arguments, "repo_map")?;
            map.expand(args.path.as_deref())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn labels(map: &RepoMap, path: &str) -> Vec<String> {
        map.symbols(path)
            .unwrap()
            .iter()
            .map(|symbol| symbol.label.clone())
            .collect()
    }

    fn fresh(root: &Path) -> RepoMap {
        RepoMap {
            root: root.to_path_buf(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn rust_top_level_symbols_and_impl_methods() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "src/pool.rs",
            "use std::fmt;\n\npub const POOL_SIZE: usize = 8;\n\n/// A pool.\npub struct Pool<T> {\n    items: Vec<T>,\n}\n\n\
             impl<T: Clone> Pool<T> {\n    pub fn new(size: usize) -> Self {\n        todo!()\n    }\n    fn grow(&mut self) {}\n}\n\n\
             impl<T> fmt::Debug for Pool<T> {\n    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { Ok(()) }\n}\n\n\
             pub trait Spawn {\n    fn spawn(&self) -> usize;\n}\n\npub enum Mode { A, B }\n\nmacro_rules! pool { () => {} }\n\n\
             pub async fn run(pool: &Pool<u8>) -> Result<(), String> {\n    Ok(())\n}\n",
        );
        let map = fresh(tmp.path());
        assert_eq!(
            labels(&map, "src/pool.rs"),
            vec![
                "const POOL_SIZE",
                "struct Pool",
                "impl Pool",
                "impl fmt::Debug for Pool",
                "trait Spawn",
                "enum Mode",
                "macro pool!",
                "fn run"
            ]
        );
        let symbols = map.symbols("src/pool.rs").unwrap();
        let inherent = &symbols[2];
        assert_eq!(inherent.signature, "impl<T: Clone> Pool<T>");
        assert_eq!(
            inherent
                .members
                .iter()
                .map(|member| (member.signature.as_str(), member.line))
                .collect::<Vec<_>>(),
            vec![
                ("pub fn new(size: usize) -> Self", 11),
                ("fn grow(&mut self)", 14)
            ]
        );
        assert_eq!(symbols[4].members[0].signature, "fn spawn(&self) -> usize;");
        assert_eq!(
            symbols[7].signature,
            "pub async fn run(pool: &Pool<u8>) -> Result<(), String>"
        );
    }

    /// `mod x;` and `mod tests` are not definitions a task looks for;
    /// an inline module is. Public items lead a cut line.
    #[test]
    fn module_declarations_and_tests_stay_out() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "src/lib.rs",
            "mod helpers;\npub mod pool;\nfn private_helper() {}\npub mod inline {\n    pub fn x() {}\n}\npub fn run() {}\n\
             #[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
        );
        let map = fresh(tmp.path());
        assert_eq!(
            labels(&map, "src/lib.rs"),
            vec!["fn private_helper", "mod inline", "fn run"]
        );
        let line = file_line("src/lib.rs", &map.symbols("src/lib.rs").unwrap(), 220);
        assert_eq!(line, "src/lib.rs: mod inline; fn run; fn private_helper");
    }

    #[test]
    fn python_go_and_typescript_outlines() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "net/retry.py",
            "import time\n\nMAX_DELAY = 300\nlower = 1\n\n@cache\ndef compute_delay(attempt: int) -> float:\n    return 1.0\n\n\
             class Backoff(Base):\n    def __init__(self):\n        pass\n\n    @property\n    def ceiling(self):\n        return 1\n",
        );
        write(
            tmp.path(),
            "cmd/main.go",
            "package main\n\nconst Limit = 3\n\ntype Server struct {\n\tport int\n}\n\n\
             func (s *Server) Start(ctx context.Context) error {\n\treturn nil\n}\n\nfunc main() {\n}\n",
        );
        write(
            tmp.path(),
            "web/api.ts",
            "export interface Options { retries: number }\nexport type Id = string;\n\
             export const DEFAULT_TIMEOUT = 30;\nexport class Client {\n  constructor(private base: string) {}\n  async fetch(id: Id): Promise<string> { return '' }\n}\n\
             export function connect(options: Options): Client {\n  return new Client('')\n}\nenum Color { Red }\n",
        );
        let map = fresh(tmp.path());
        assert_eq!(
            labels(&map, "net/retry.py"),
            vec!["MAX_DELAY", "def compute_delay", "class Backoff"]
        );
        let python = map.symbols("net/retry.py").unwrap();
        assert_eq!(
            python[1].signature,
            "def compute_delay(attempt: int) -> float"
        );
        assert_eq!(
            python[2]
                .members
                .iter()
                .map(|member| member.label.as_str())
                .collect::<Vec<_>>(),
            vec!["def __init__", "def ceiling"]
        );
        assert_eq!(
            labels(&map, "cmd/main.go"),
            vec![
                "const Limit",
                "type Server",
                "func Server.Start",
                "func main"
            ]
        );
        assert_eq!(
            labels(&map, "web/api.ts"),
            vec![
                "interface Options",
                "type Id",
                "const DEFAULT_TIMEOUT",
                "class Client",
                "function connect",
                "enum Color"
            ]
        );
        let client = &map.symbols("web/api.ts").unwrap()[3];
        assert_eq!(
            client
                .members
                .iter()
                .map(|member| member.label.as_str())
                .collect::<Vec<_>>(),
            vec!["constructor", "fetch"]
        );
    }

    /// The cache keys on the blob hash: an unchanged file is not
    /// re-parsed, a changed one is.
    #[test]
    fn a_changed_file_reparses_and_an_unchanged_one_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs", "fn one() {}\n");
        let map = fresh(tmp.path());
        let first = map.symbols("a.rs").unwrap();
        let again = map.symbols("a.rs").unwrap();
        assert!(Arc::ptr_eq(&first, &again), "unchanged: the cached outline");
        write(tmp.path(), "a.rs", "fn one() {}\nfn two() {}\n");
        assert_eq!(labels(&map, "a.rs"), vec!["fn one", "fn two"]);
    }

    /// Named, touched and cited files lead; the walk skips build output
    /// and dependencies; the summary counts the rest.
    #[test]
    fn the_framing_ranks_for_the_session_and_skips_build_output() {
        let tmp = tempfile::tempdir().unwrap();
        for index in 0..5 {
            write(tmp.path(), &format!("src/m{index}.rs"), "pub fn f() {}\n");
        }
        write(
            tmp.path(),
            "src/deep/retry.rs",
            "pub fn compute_delay() {}\n",
        );
        write(tmp.path(), "src/deep/named.rs", "pub fn named() {}\n");
        write(tmp.path(), "src/deep/cited.rs", "pub fn cited() {}\n");
        write(tmp.path(), "target/debug/build.rs", "fn generated() {}\n");
        write(tmp.path(), "node_modules/x/index.js", "function x() {}\n");
        write(tmp.path(), "README.md", "# not source\n");
        let map = fresh(tmp.path());
        let body = map
            .framing(
                &MapHints {
                    touched: Vec::new(),
                    cited: vec!["src/deep/cited.rs".into()],
                    said: "fix src/deep/named.rs and the retry delay".into(),
                },
                500,
            )
            .unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert!(
            lines[0].starts_with("src/deep/named.rs: fn named"),
            "{body}"
        );
        assert!(lines[1].starts_with("src/deep/cited.rs"), "{body}");
        assert!(
            lines[2].starts_with("src/deep/retry.rs: fn compute_delay"),
            "{body}"
        );
        assert!(
            !body.contains("target/") && !body.contains("node_modules"),
            "{body}"
        );
        assert!(!body.contains("README"), "{body}");
        assert_eq!(lines.len(), 8, "every file fits, no summary: {body}");
    }

    /// The budget is a hard ceiling, and a huge repo degrades to the
    /// summary line.
    #[test]
    fn the_framing_never_exceeds_its_budget() {
        let tmp = tempfile::tempdir().unwrap();
        for index in 0..300 {
            write(
                tmp.path(),
                &format!("pkg{}/module_{index}.py", index % 7),
                "def handler(request):\n    pass\n\nclass Service:\n    pass\n",
            );
        }
        let map = fresh(tmp.path());
        for budget in [20, 60, 200, 500, 2_000] {
            let body = map.framing(&MapHints::default(), budget).unwrap();
            assert!(
                body.len() <= budget * 4,
                "budget {budget}: {} chars\n{body}",
                body.len()
            );
            assert!(body.contains("more source files"), "{body}");
        }
        // Too small for any file line: the summary alone.
        let tiny = map.framing(&MapHints::default(), 40).unwrap();
        assert_eq!(tiny.lines().count(), 1, "{tiny}");
        assert!(tiny.starts_with("… 300 more source files"), "{tiny}");
    }

    #[test]
    fn the_tool_lists_directories_files_and_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/lib.rs", "pub mod pool;\npub fn run() {}\n");
        write(
            tmp.path(),
            "src/pool.rs",
            "pub struct Pool;\nimpl Pool {\n    pub fn new() -> Self { Pool }\n}\n",
        );
        write(tmp.path(), "build.py", "def main():\n    pass\n");
        write(tmp.path(), "notes.txt", "hello\n");
        let map = fresh(tmp.path());
        let overview = map.expand(None).unwrap();
        assert!(overview.contains("3 source files"), "{overview}");
        assert!(
            overview.contains("src/ 2 files") && overview.contains("build.py"),
            "{overview}"
        );
        let directory = map.expand(Some("src/")).unwrap();
        assert!(
            directory.contains("src/pool.rs: struct Pool; impl Pool"),
            "{directory}"
        );
        let file = map.expand(Some("./src/pool.rs")).unwrap();
        assert!(file.contains("L1 pub struct Pool;"), "{file}");
        assert!(file.contains("  L3 pub fn new() -> Self"), "{file}");
        assert!(
            map.expand(Some("notes.txt"))
                .unwrap_err()
                .contains("read_file")
        );
        assert!(map.expand(Some("missing")).is_err());
        assert!(
            map.expand(Some("../outside")).is_err(),
            "contained to the workspace"
        );
    }
}
