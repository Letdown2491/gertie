//! Markdown rendering for the transcript.
//!
//! A streaming coding agent emits real markdown, headings, lists, tables,
//! fences, emphasis, links, and reading raw asterisks in the chat pane is
//! hostile. This module parses blocks (line-based, tolerant of partial
//! input) and renders them as native elements; inline spans become styled
//! text runs, so `**bold**` looks bold *while it streams*, not only after.
//! Markers are stripped, not styled in place: the display text is the
//! source minus its syntax, with highlight ranges renumbered to match.
//!
//! Deliberately not a CommonMark engine: no reference links, no setext
//! headings, no nested-block arbitrary depth. It covers what models
//! actually emit, stays safe on unclosed constructs (an unclosed fence
//! renders its tail as code, mid-stream that is exactly what it is), and
//! costs no dependency.

use std::ops::Range;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, FontStyle, HighlightStyle, StyledText, UnderlineStyle, div, prelude::*, px,
};

// ---------------------------------------------------------------------------
// Block model + parser
// ---------------------------------------------------------------------------

/// A parsed block. Text is kept raw; inline parsing happens at render time
/// (so the parser is pure and unit-testable without a theme).
#[derive(Debug, PartialEq)]
pub enum Block {
    /// Soft-joined prose (lines merge with single spaces, per markdown).
    Paragraph(String),
    /// `#`–`######` ATX heading.
    Heading { level: u8, text: String },
    /// Fenced code; an unclosed fence at EOF still yields a Code block.
    Code {
        language: Option<String>,
        body: String,
    },
    /// `>` blockquote; inner text is re-parsed as blocks on render.
    Quote(String),
    /// Consecutive list items, nesting by indent level.
    List(Vec<ListItem>),
    /// A `|`-delimited table with a `---` separator row.
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// `---`, `***`, or `___` alone on a line.
    Rule,
}

#[derive(Debug, PartialEq)]
pub struct ListItem {
    /// Nesting depth from leading whitespace (2 spaces per level).
    pub level: usize,
    /// Ordered (`1.`) vs bullet (`-`/`*`/`+`).
    pub ordered: bool,
    /// The item's own number when ordered (rendered verbatim).
    pub number: u64,
    /// Marker text for task lists: Some("x") for `- [x]`, Some(" ") for `- [ ]`.
    pub task: Option<TaskMark>,
    pub text: String,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TaskMark {
    Done,
    Open,
}

pub fn parse_blocks(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Blank lines separate blocks; nothing else to do with them.
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code: everything until the closing fence is code, and an
        // EOF without a close still yields the block (streaming-safe).
        if trimmed.starts_with("```") {
            let language = trimmed
                .strip_prefix("```")
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(String::from);
            let mut body = String::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                body.push_str(lines[i]);
                body.push('\n');
                i += 1;
            }
            i += 1; // the closing fence, if any
            blocks.push(Block::Code { language, body });
            continue;
        }

        // Heading: 1-6 `#` then a space.
        if let Some(level) = heading_level(trimmed) {
            blocks.push(Block::Heading {
                level,
                text: trimmed[level as usize..].trim().to_string(),
            });
            i += 1;
            continue;
        }

        // Horizontal rule: 3+ of one punctuation char, alone.
        if is_rule(trimmed) {
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }

        // Blockquote: consecutive `>` lines, marker stripped. The strip
        // must always consume the `>`, falling back to the original row
        // would leave the inner text starting with `>`, and rendering
        // re-parses quote content, recursing forever (the stack overflow
        // of Oct 25). Nested markers flatten to one level.
        if trimmed.starts_with('>') {
            let mut inner = String::new();
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                // Flatten every level of `>`-nesting (and the space after
                // it) in one pass.
                let after = lines[i].trim_start_matches(['>', ' ']);
                inner.push_str(after);
                inner.push('\n');
                i += 1;
            }
            blocks.push(Block::Quote(inner));
            continue;
        }

        // Table: a `|` row followed by a `|---|` separator row.
        if trimmed.starts_with('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let header = table_cells(trimmed);
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() {
                let row = lines[i].trim();
                if !row.starts_with('|') {
                    break;
                }
                rows.push(table_cells(row));
                i += 1;
            }
            blocks.push(Block::Table { header, rows });
            continue;
        }

        // List: consecutive item lines (plus indented continuations).
        // The raw line goes in, `list_item` reads the indent itself to
        // derive the nesting level.
        if let Some(item) = list_item(line) {
            let mut items = vec![item];
            i += 1;
            while i < lines.len() {
                let row = lines[i];
                let row_trimmed = row.trim_start();
                if row_trimmed.is_empty() {
                    // A blank line only continues the list if another item
                    // follows; otherwise the list is over.
                    let more = lines
                        .get(i + 1)
                        .is_some_and(|next| list_item(next).is_some());
                    if !more {
                        break;
                    }
                    i += 1;
                    continue;
                }
                if let Some(next) = list_item(row) {
                    items.push(next);
                } else if row.starts_with(' ') {
                    // Continuation of the previous item's text.
                    if let Some(last) = items.last_mut() {
                        last.text.push(' ');
                        last.text.push_str(row_trimmed);
                    }
                } else {
                    break;
                }
                i += 1;
            }
            blocks.push(Block::List(items));
            continue;
        }

        // Paragraph: accumulate until a blank line or the start of
        // another block. The first line is consumed unconditionally,
        // a line that "starts a block" no branch handled (a `|` row
        // with no separator line after it) is just paragraph text.
        let mut prose = String::new();
        while let Some(row) = lines.get(i) {
            let row_trimmed = row.trim_start();
            let starts_block = row_trimmed.is_empty()
                || row_trimmed.starts_with("```")
                || heading_level(row_trimmed).is_some()
                || is_rule(row_trimmed)
                || row_trimmed.starts_with('>')
                || row_trimmed.starts_with('|')
                || list_item(row).is_some();
            if starts_block && !prose.is_empty() {
                break;
            }
            if !prose.is_empty() {
                prose.push(' ');
            }
            prose.push_str(row_trimmed);
            i += 1;
        }
        if !prose.is_empty() {
            blocks.push(Block::Paragraph(prose));
        }
    }

    blocks
}

fn heading_level(line: &str) -> Option<u8> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes)
        && line[hashes..]
            .chars()
            .next()
            .is_some_and(|c| c == ' ' || c == '\t')
    {
        Some(hashes as u8)
    } else {
        None
    }
}

fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|c| trimmed.chars().all(|ch| ch == *c))
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return false;
    }
    trimmed.trim_matches('|').split('|').all(|cell| {
        let cell = cell.trim();
        !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':' || c == ' ')
    }) && trimmed.contains('-')
}

/// Splits a `| a | b |` row into cells, dropping the empty edge cells.
fn table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// One list-item line: `- text`, `* text`, `+ text`, `1. text`, with
/// optional `[x]`/`[ ]` task marker.
fn list_item(line: &str) -> Option<ListItem> {
    let indent = line.len() - line.trim_start().len();
    if indent > 6 {
        return None; // deep indents are code blocks per markdown; don't nest
    }
    let rest = line.trim_start();

    let (ordered, number, after_marker) =
        if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
            (false, 0, &rest[2..])
        } else if rest == "-" || rest == "*" || rest == "+" {
            (false, 0, "")
        } else {
            let digits: usize = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digits == 0 {
                return None;
            }
            let marker = &rest[digits..];
            if !marker.starts_with(". ") && !marker.starts_with(") ") {
                return None;
            }
            let number: u64 = rest[..digits].parse().ok()?;
            (true, number, &marker[2..])
        };

    let (task, text) = if let Some(task) = task_marker(after_marker) {
        task
    } else {
        (None, after_marker.trim_end())
    };

    Some(ListItem {
        level: indent / 2,
        ordered,
        number,
        task,
        text: text.to_string(),
    })
}

fn task_marker(text: &str) -> Option<(Option<TaskMark>, &str)> {
    if let Some(rest) = text.strip_prefix("[x] ") {
        Some((Some(TaskMark::Done), rest))
    } else if let Some(rest) = text.strip_prefix("[X] ") {
        Some((Some(TaskMark::Done), rest))
    } else if let Some(rest) = text.strip_prefix("[ ] ") {
        Some((Some(TaskMark::Open), rest))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Inline spans
// ---------------------------------------------------------------------------

/// Which inline treatment a range gets. Parsed purely, mapped to
/// `HighlightStyle` at render time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Plain,
    Bold,
    Italic,
    BoldItalic,
    Code,
    Strike,
    Link,
}

#[derive(Clone, Copy, PartialEq)]
struct Flags {
    bold: bool,
    italic: bool,
    link: bool,
}

impl Flags {
    const PLAIN: Self = Self {
        bold: false,
        italic: false,
        link: false,
    };
    fn kind(self) -> SpanKind {
        match (self.bold, self.italic, self.link) {
            (true, true, _) => SpanKind::BoldItalic,
            (true, false, _) => SpanKind::Bold,
            (false, true, _) => SpanKind::Italic,
            (false, false, true) => SpanKind::Link,
            (false, false, false) => SpanKind::Plain,
        }
    }
}

/// Scans one run of text for inline markers. Markers are *stripped*: the
/// cleaned text is appended to `out_text` and the returned ranges index
/// into it, so `**bold**` displays as `bold` with a bold range. Unmarked
/// runs flush as maximal spans carrying the current flags, that is how
/// plain text inside `**…**` or `[…](…)` inherits its treatment, and
/// with no flags active they push nothing: plain is the absence of
/// styling. Links and code suppress nesting inside them.
fn scan_spans(
    text: &str,
    flags: Flags,
    out_text: &mut String,
    out: &mut Vec<(Range<usize>, SpanKind)>,
) {
    let bytes = text.as_bytes();
    let mut i = 0;
    // Where the current unmarked run begins, in out_text coordinates.
    let mut run_start = out_text.len();
    // Runs flush only when they'd carry a treatment; with no flags
    // active, plain text stays span-free.
    let styled = flags != Flags::PLAIN;

    while i < bytes.len() {
        let rest = &text[i..];

        // Inline code: the ticks drop out; the span between them is kept
        // verbatim, no nesting inside.
        if bytes[i] == b'`'
            && let Some(close) = rest[1..].find('`')
        {
            if styled && out_text.len() > run_start {
                out.push((run_start..out_text.len(), flags.kind()));
            }
            let content = &rest[1..1 + close];
            if !content.is_empty() {
                let start = out_text.len();
                out_text.push_str(content);
                out.push((start..out_text.len(), SpanKind::Code));
            }
            i += 1 + close + 1;
            run_start = out_text.len();
            continue;
        }

        // **bold** / __bold__: the markers drop; the inner scan inherits
        // italic and link.
        if (rest.starts_with("**") || rest.starts_with("__"))
            && let Some(close) = rest[2..].find(&rest[..2])
        {
            if styled && out_text.len() > run_start {
                out.push((run_start..out_text.len(), flags.kind()));
            }
            let inner = &rest[2..2 + close];
            // Emphasis content neither starts nor ends with a space,
            // `** bold**` is literal text, per CommonMark.
            if !inner.is_empty() && !inner.starts_with(' ') && !inner.ends_with(' ') {
                let inner_flags = if flags.link {
                    flags
                } else {
                    Flags {
                        bold: true,
                        ..flags
                    }
                };
                scan_spans(inner, inner_flags, out_text, out);
            } else {
                // Degenerate (`****`) or space-padded: verbatim, and it
                // joins the surrounding run.
                out_text.push_str(&rest[..2 + close + 2]);
            }
            i += 2 + close + 2;
            run_start = out_text.len();
            continue;
        }

        // ~~strikethrough~~: atomic, the outer markers drop, the inner
        // text keeps whatever markers it has, struck through whole.
        if rest.starts_with("~~")
            && let Some(close) = rest[2..].find("~~")
            && close > 0
        {
            if styled && out_text.len() > run_start {
                out.push((run_start..out_text.len(), flags.kind()));
            }
            let start = out_text.len();
            out_text.push_str(&rest[2..2 + close]);
            out.push((start..out_text.len(), SpanKind::Strike));
            i += 2 + close + 2;
            run_start = out_text.len();
            continue;
        }

        // *italic* / _italic_: `_` only at word boundaries, so
        // snake_case survives as plain text. `close` is the closer's
        // index in `rest`: the inner text is `rest[1..close]`, and the
        // closer is one char, `close + 1` consumes it and nothing more.
        if (bytes[i] == b'*' || bytes[i] == b'_')
            && opens_emphasis(text, i, bytes[i])
            && let Some(close) = emphasis_close(rest, bytes[i])
        {
            if styled && out_text.len() > run_start {
                out.push((run_start..out_text.len(), flags.kind()));
            }
            let inner_flags = if flags.link {
                flags
            } else {
                Flags {
                    bold: flags.bold,
                    italic: true,
                    link: false,
                }
            };
            scan_spans(&rest[1..close], inner_flags, out_text, out);
            i += close + 1;
            run_start = out_text.len();
            continue;
        }

        // Escaped punctuation: the backslash drops, the char stays, as
        // part of the surrounding run, so an escaped `*` inside bold
        // stays bold.
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let char_len = text[i + 1..].chars().next().map_or(1, char::len_utf8);
            out_text.push_str(&text[i + 1..i + 1 + char_len]);
            i += 1 + char_len;
            continue;
        }

        // [text](url): the text stays (with its inner markers, styled as
        // a link); the brackets and url plumbing drop.
        if bytes[i] == b'['
            && let Some((text_end, total)) = parse_link(rest)
        {
            if styled && out_text.len() > run_start {
                out.push((run_start..out_text.len(), flags.kind()));
            }
            scan_spans(
                &rest[1..text_end],
                Flags {
                    link: true,
                    ..flags
                },
                out_text,
                out,
            );
            i += total;
            run_start = out_text.len();
            continue;
        }

        // Bare URLs after a boundary link to the next whitespace.
        if (rest.starts_with("http://") || rest.starts_with("https://"))
            && (i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t' || bytes[i - 1] == b'(')
            && let Some(end) = rest.find(|c: char| c.is_whitespace() || c == ')')
            && end > 0
        {
            if styled && out_text.len() > run_start {
                out.push((run_start..out_text.len(), flags.kind()));
            }
            let start = out_text.len();
            out_text.push_str(&rest[..end]);
            out.push((start..out_text.len(), SpanKind::Link));
            i += end;
            run_start = out_text.len();
            continue;
        }

        // Fall through: copy one whole character, a byte-wise advance
        // would slice multi-byte characters and panic on the next &text[i..].
        let char_len = text[i..].chars().next().map_or(1, char::len_utf8);
        out_text.push_str(&text[i..i + char_len]);
        i += char_len;
    }
    if styled && out_text.len() > run_start {
        out.push((run_start..out_text.len(), flags.kind()));
    }
}

/// `[text](url)` → (end of text, total consumed).
fn parse_link(rest: &str) -> Option<(usize, usize)> {
    let text_end = rest.find(']')?;
    if text_end == 0 || !rest[text_end + 1..].starts_with('(') {
        return None;
    }
    let url_end = rest[text_end + 2..].find(')')? + text_end + 2;
    if url_end == text_end + 2 {
        return None; // empty url
    }
    Some((text_end, url_end + 1))
}

/// The display text with inline markers stripped, plus styled ranges
/// into it. This is the single source of truth for what a run of prose
/// shows and how.
pub fn scan_inline(text: &str) -> (String, Vec<(Range<usize>, SpanKind)>) {
    let mut out_text = String::with_capacity(text.len());
    let mut spans = Vec::new();
    scan_spans(text, Flags::PLAIN, &mut out_text, &mut spans);
    (out_text, spans)
}

/// `_` counts only when not glued to a word on either side.
fn opens_emphasis(text: &str, at: usize, marker: u8) -> bool {
    let prev_is_word = text[..at]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric());
    let next_is_word = text[at + 1..]
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric());
    if marker == b'_' {
        !prev_is_word && next_is_word
    } else {
        next_is_word || !prev_is_word
    }
}

/// Distance from `rest[1..]` to the matching emphasis closer, rejecting
/// empty and whitespace-opened spans. Doubled markers (`**`) are skipped
/// rather than treated as closers, they belong to the bold pass, and
/// `*em **strong** tail*` must close on the final `*`.
fn emphasis_close(rest: &str, marker: u8) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut search = 1;
    let found = loop {
        let candidate = rest[search..].find(marker as char)? + search;
        if bytes.get(candidate + 1) == Some(&marker) {
            // First half of a doubled pair: keep looking past both.
            search = candidate + 2;
            continue;
        }
        break candidate;
    };
    if found <= 1 || rest[1..found].starts_with(char::is_whitespace) {
        return None;
    }
    // A `_` closer glued to a word (file_format_) isn't a closer.
    if marker == b'_'
        && rest[found + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric())
    {
        return None;
    }
    Some(found)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Renders markdown text as one element: blocks stacked with breathing
/// room between them, a wall of flush paragraphs reads like a log, not
/// prose.
pub fn render(text: &str, cx: &App) -> AnyElement {
    let blocks = parse_blocks(text)
        .iter()
        .filter(|block| !matches!(block, Block::Paragraph(p) if p.trim().is_empty()))
        .map(|block| render_block(block, cx))
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        div().into_any_element()
    } else {
        v_flex()
            .w_full()
            .gap_2()
            .children(blocks)
            .into_any_element()
    }
}

/// Per-row parse memoization for the transcript. The view re-renders
/// every frame, and each frame used to re-parse every visible message,
/// during streaming, the whole parser ran on every row on every token.
/// Turn text is append-only, so a (row, text length) match means the
/// parse is still exactly this text's: the growing row re-parses per
/// throttled frame, every settled row parses once, ever.
#[derive(Default)]
pub struct ParseCache {
    rows: std::collections::HashMap<u64, (usize, std::rc::Rc<Vec<Block>>)>,
}

impl ParseCache {
    /// Render with a memoized parse. `key` identifies the row (the
    /// turn index; a reload clears the cache, so identities may be
    /// reused after one).
    pub fn render(&mut self, key: u64, text: &str, cx: &App) -> AnyElement {
        if self.rows.len() > 1024 {
            // "Load earlier" reindexes every row; the bound keeps a
            // long-lived session's map honest without tuning.
            self.rows.clear();
        }
        let blocks = match self.rows.get(&key) {
            Some((len, blocks)) if *len == text.len() => blocks.clone(),
            Some(_) | None => {
                let blocks = std::rc::Rc::new(parse_blocks(text));
                self.rows.insert(key, (text.len(), blocks.clone()));
                blocks
            }
        };
        render_parsed(&blocks, cx)
    }

    /// A reload replaces every turn, no cached parse can survive it.
    pub fn clear(&mut self) {
        self.rows.clear();
    }
}

/// The element-building half of [`render`], over an already-parsed
/// block list.
fn render_parsed(blocks: &[Block], cx: &App) -> AnyElement {
    let elements = blocks
        .iter()
        .filter(|block| !matches!(block, Block::Paragraph(p) if p.trim().is_empty()))
        .map(|block| render_block(block, cx))
        .collect::<Vec<_>>();
    if elements.is_empty() {
        div().into_any_element()
    } else {
        v_flex()
            .w_full()
            .gap_2()
            .children(elements)
            .into_any_element()
    }
}

fn render_block(block: &Block, cx: &App) -> AnyElement {
    let theme = cx.theme();
    match block {
        Block::Paragraph(text) => inline_element(text, cx),
        Block::Heading { level, text } => {
            let size = match level {
                1 => 20.,
                2 => 18.,
                3 => 16.,
                4 => 15.,
                _ => 14.5,
            };
            div()
                .w_full()
                .text_size(px(size))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(inline_element(text, cx))
                .into_any_element()
        }
        Block::Code { language, body } => code_block(language.as_deref(), body, cx),
        Block::Quote(inner) => v_flex()
            .w_full()
            .gap_1()
            .pl_3()
            .border_l_2()
            .border_color(theme.border)
            .text_color(theme.muted_foreground)
            .child(render(inner, cx))
            .into_any_element(),
        Block::List(items) => v_flex()
            .w_full()
            .gap_1()
            .children(items.iter().map(|item| render_list_item(item, cx)))
            .into_any_element(),
        Block::Table { header, rows } => render_table(header, rows, cx),
        Block::Rule => div()
            .w_full()
            .my_1()
            .h(px(1.))
            .flex_shrink_0()
            .bg(theme.border)
            .into_any_element(),
    }
}

fn render_list_item(item: &ListItem, cx: &App) -> AnyElement {
    let marker = match item.task {
        Some(TaskMark::Done) => "✓".to_string(),
        Some(TaskMark::Open) => "○".to_string(),
        None if item.ordered => format!("{}.", item.number),
        None => "•".to_string(),
    };
    h_flex()
        .w_full()
        .gap_2()
        // Nested items indent one step each level.
        .pl(px((item.level.min(4) * 16) as f32))
        .child(
            div()
                .w(px(24.))
                .flex_shrink_0()
                .text_align(gpui_kit::TextAlign::Right)
                .text_color(cx.theme().muted_foreground)
                .child(marker),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(inline_element(&item.text, cx)),
        )
        .into_any_element()
}

fn render_table(header: &[String], rows: &[Vec<String>], cx: &App) -> AnyElement {
    let theme = cx.theme();
    let columns = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0))
        .min(8);
    if columns == 0 {
        return div().into_any_element();
    }

    let cells = |cells: &[String], header_row: bool| {
        let theme = cx.theme();
        (0..columns)
            .map(|column| {
                let text = cells.get(column).cloned().unwrap_or_default();
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .when(header_row, |cell| {
                        cell.font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .bg(theme.muted.opacity(0.5))
                    })
                    .child(inline_element(&text, cx))
            })
            .collect::<Vec<_>>()
    };

    let mut table = v_flex()
        .w_full()
        .my_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(h_flex().w_full().children(cells(header, true)));

    for row in rows {
        table = table.child(
            h_flex()
                .w_full()
                .border_t_1()
                .border_color(theme.border)
                .children(cells(row, false)),
        );
    }
    table.into_any_element()
}

/// One line of prose with inline markers resolved, headings, list items,
/// and tool-card labels all flow through here.
pub fn inline_element(text: &str, cx: &App) -> AnyElement {
    if text.is_empty() {
        return div().into_any_element();
    }
    let theme = cx.theme();
    let (clean, spans) = scan_inline(text);
    let highlights: Vec<(Range<usize>, HighlightStyle)> = spans
        .into_iter()
        .filter_map(|(range, kind)| {
            let style = match kind {
                SpanKind::Plain => None,
                SpanKind::Bold => Some(HighlightStyle {
                    font_weight: Some(gpui_kit::FontWeight::BOLD),
                    ..Default::default()
                }),
                SpanKind::Italic => Some(HighlightStyle {
                    font_style: Some(FontStyle::Italic),
                    ..Default::default()
                }),
                SpanKind::BoldItalic => Some(HighlightStyle {
                    font_weight: Some(gpui_kit::FontWeight::BOLD),
                    font_style: Some(FontStyle::Italic),
                    ..Default::default()
                }),
                // Inline code keeps the surrounding font (HighlightStyle
                // can't swap families) but gets a chip: tinted background,
                // plain foreground, the accent on muted pairs badly in
                // the light theme (the word vanished into the chip).
                SpanKind::Code => Some(HighlightStyle {
                    background_color: Some(theme.muted.opacity(0.6)),
                    color: Some(theme.foreground),
                    ..Default::default()
                }),
                SpanKind::Strike => Some(HighlightStyle {
                    strikethrough: Some(gpui_kit::StrikethroughStyle {
                        thickness: px(1.),
                        color: None,
                    }),
                    ..Default::default()
                }),
                SpanKind::Link => Some(HighlightStyle {
                    color: Some(theme.link),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(theme.link),
                        wavy: false,
                    }),
                    ..Default::default()
                }),
            };
            style.map(|style| (range, style))
        })
        .collect();

    if highlights.is_empty() {
        div().child(clean).into_any_element()
    } else {
        StyledText::new(clean)
            .with_highlights(highlights)
            .into_any_element()
    }
}

/// A fenced code block: mono, boxed, with the language tag when given.
fn code_block(language: Option<&str>, body: &str, cx: &App) -> AnyElement {
    v_flex()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted)
        .overflow_hidden()
        .when_some(language, |block, lang| {
            block.child(
                div()
                    .px_2()
                    .pt_1()
                    .text_size(px(10.5))
                    .text_color(cx.theme().muted_foreground)
                    .child(lang.to_string()),
            )
        })
        .child(
            div()
                .px_2()
                .py_1p5()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(cx.theme().mono_font_size)
                .child(body.trim_end_matches('\n').to_string()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_win_over_everything_and_survive_unclosed() {
        let blocks = parse_blocks("prose\n\n```rust\nlet x = 1;\n```\nafter");
        assert_eq!(
            blocks,
            vec![
                Block::Paragraph("prose".into()),
                Block::Code {
                    language: Some("rust".into()),
                    body: "let x = 1;\n".into()
                },
                Block::Paragraph("after".into()),
            ]
        );

        let streaming = parse_blocks("```python\nprint(1)");
        assert_eq!(
            streaming,
            vec![Block::Code {
                language: Some("python".into()),
                body: "print(1)\n".into()
            }]
        );
    }

    #[test]
    fn headings_rules_and_quotes() {
        let blocks = parse_blocks("# Title\n\ntext\n\n---\n\n> quoted\n> more\n");
        assert_eq!(
            blocks,
            vec![
                Block::Heading {
                    level: 1,
                    text: "Title".into()
                },
                Block::Paragraph("text".into()),
                Block::Rule,
                Block::Quote("quoted\nmore\n".into()),
            ]
        );
    }

    #[test]
    fn quotes_lose_their_marker_even_without_a_space() {
        // The invariant that bounds render's recursion: parsed quote
        // content never starts with `>`, so render → parse → Quote can
        // not loop. `>text` (no space) recursed forever before.
        for input in [">x", "> x", ">>x", ">> x", ">", "> ", "> > y"] {
            match parse_blocks(input)[..] {
                [Block::Quote(ref inner)] => {
                    assert!(!inner.starts_with('>'), "{input:?} → {inner:?}")
                }
                ref other => panic!("{input:?}: expected one quote, got {other:?}"),
            }
        }
        assert_eq!(parse_blocks(">> x")[..], [Block::Quote("x\n".into())]);
    }

    #[test]
    fn lists_nest_and_continue() {
        let blocks =
            parse_blocks("- top\n  continued\n- second\n  1. inner\n  2. inner too\ntail\n");
        let Block::List(items) = &blocks[0] else {
            panic!("expected list, got {blocks:?}");
        };
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].text, "top continued");
        assert!(!items[0].ordered);
        assert!(items[2].ordered && items[2].number == 1);
        assert_eq!(items[3].level, 1);
        // A paragraph after the list is a separate block.
        assert_eq!(blocks[1], Block::Paragraph("tail".into()));
    }

    #[test]
    fn task_lists_parse_their_marks() {
        let blocks = parse_blocks("- [x] done thing\n- [ ] pending thing\n");
        let Block::List(items) = &blocks[0] else {
            panic!("expected list");
        };
        assert_eq!(items[0].task, Some(TaskMark::Done));
        assert_eq!(items[1].task, Some(TaskMark::Open));
    }

    #[test]
    fn tables_need_a_separator_row() {
        let plain = parse_blocks("| a | b |\nnot a table");
        assert_eq!(plain[0], Block::Paragraph("| a | b | not a table".into()));

        let table = parse_blocks("| a | b |\n|---|:-:|\n| 1 | 2 |\n| 3 | 4 |");
        assert_eq!(
            table[0],
            Block::Table {
                header: vec!["a".into(), "b".into()],
                rows: vec![vec!["1".into(), "2".into()], vec!["3".into(), "4".into()]],
            }
        );
    }

    #[test]
    fn inline_scanner_bolds_italics_and_codes() {
        let text = "plain **bold** and *em* and `code` and ~~gone~~";
        let (clean, spans) = scan_inline(text);
        // The markers are gone from the display text.
        assert_eq!(clean, "plain bold and em and code and gone");
        assert!(spans.iter().any(|s| s.1 == SpanKind::Bold));
        assert!(spans.iter().any(|s| s.1 == SpanKind::Italic));
        assert!(spans.iter().any(|s| s.1 == SpanKind::Code));
        assert!(spans.iter().any(|s| s.1 == SpanKind::Strike));

        for (range, _) in &spans {
            assert!(clean.is_char_boundary(range.start) && clean.is_char_boundary(range.end));
        }
        let bold = spans.iter().find(|s| s.1 == SpanKind::Bold).unwrap();
        assert_eq!(&clean[bold.0.clone()], "bold");
        let code = spans.iter().find(|s| s.1 == SpanKind::Code).unwrap();
        assert_eq!(&clean[code.0.clone()], "code");
    }

    #[test]
    fn markers_are_stripped_not_styled_in_place() {
        // The whole-answer case: a paragraph that opens bold shows its
        // words, never its asterisks.
        let (clean, spans) = scan_inline("**Fleet-grade safety** first");
        assert_eq!(clean, "Fleet-grade safety first");
        assert_eq!(spans[0].1, SpanKind::Bold);
        assert_eq!(&clean[spans[0].0.clone()], "Fleet-grade safety");

        // A link's plumbing drops entirely, text and destination stay.
        let (clean, spans) = scan_inline("see [the docs](https://example.com/x) now");
        assert_eq!(clean, "see the docs now");
        let links: Vec<&str> = spans
            .iter()
            .filter(|(_, kind)| *kind == SpanKind::Link)
            .map(|(range, _)| &clean[range.clone()])
            .collect();
        assert_eq!(links, vec!["the docs"]);
    }

    #[test]
    fn streaming_and_degenerate_markers_stay_verbatim() {
        // Mid-stream (`**Fleet`), degenerate (`****`), space-padded, and
        // an empty code span all render exactly what arrived.
        for text in ["**Fleet", "****", "** bold**", "` ", "a ` b"] {
            let (clean, spans) = scan_inline(text);
            assert_eq!(clean, text, "{text:?} should pass through untouched");
            assert!(spans.is_empty(), "{text:?} should have no spans: {spans:?}");
        }
    }

    #[test]
    fn snake_case_is_not_italic() {
        let (clean, spans) = scan_inline("call some_function_name with args");
        assert_eq!(clean, "call some_function_name with args");
        assert!(spans.is_empty());
    }

    #[test]
    fn links_parse_and_bare_urls_autolink() {
        let text = "see [the docs](https://example.com/x) or https://example.com now";
        let (clean, spans) = scan_inline(text);
        assert_eq!(clean, "see the docs or https://example.com now");
        let links: Vec<&str> = spans
            .iter()
            .filter(|(_, kind)| *kind == SpanKind::Link)
            .map(|(range, _)| &clean[range.clone()])
            .collect();
        assert_eq!(links, vec!["the docs", "https://example.com"]);
    }

    #[test]
    fn unclosed_markers_stay_literal() {
        for text in ["a ** b", "a * b", "a ` b", "a ~~ b", "a [link"] {
            let (clean, spans) = scan_inline(text);
            assert_eq!(clean, text, "{text:?} should be verbatim");
            assert!(
                spans.iter().all(|(_, kind)| *kind == SpanKind::Plain),
                "{text:?} should be plain: {spans:?}"
            );
        }
    }

    #[test]
    fn bold_inside_italic_nests() {
        let text = "*em **strong** tail*";
        let (clean, spans) = scan_inline(text);
        assert_eq!(clean, "em strong tail");
        let strong = spans
            .iter()
            .find(|(_, kind)| *kind == SpanKind::BoldItalic)
            .expect("inner bold inherits italic");
        assert_eq!(&clean[strong.0.clone()], "strong");
    }

    #[test]
    fn inline_code_shields_its_contents() {
        let (clean, spans) = scan_inline("`**not bold**`");
        // Code is literal: the ticks drop, the asterisks survive.
        assert_eq!(clean, "**not bold**");
        assert!(spans.iter().all(|(_, kind)| *kind == SpanKind::Code));
    }

    #[test]
    fn multi_byte_characters_walk_by_whole_char() {
        // A byte-wise advance through '…', 'é', or an emoji would slice
        // mid-character and panic; the scanner must step whole chars.
        for text in [
            "…set the key",
            "étoile *em* étoile",
            "🦀 **bold** 🦀",
            "café, *emphasised*, naïve",
            "\\*escaped* then … more",
        ] {
            let (clean, spans) = scan_inline(text);
            for (range, _) in &spans {
                assert!(
                    clean.is_char_boundary(range.start) && clean.is_char_boundary(range.end),
                    "{text:?} span {range:?} crosses a char boundary"
                );
            }
        }
        let (clean, spans) = scan_inline("plain … **bold**");
        assert_eq!(clean, "plain … bold");
        assert!(spans.iter().any(|(_, kind)| *kind == SpanKind::Bold));
    }
}
