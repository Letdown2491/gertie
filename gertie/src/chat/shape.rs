//! The turn's shape (issue #54): what each tool call was for, how far
//! the turn has come (the stages Gertie's face and ring show), and which
//! calls are numbered
//! work items (the thread marks shared by the words pane, the seam and
//! the work pane): one per edited file, one per command. Pure: the
//! panes render from it, tests read it.

/// What a tool call was for, as the two panes see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Looking: reads, searches, maps, fetches.
    Explore,
    /// Consulting or writing memory.
    Memory,
    /// A question for the human.
    Ask,
    /// A file change.
    Edit,
    /// A test, check, lint or build command.
    Check,
    /// Any other shell command.
    Run,
    /// A subagent run.
    Worker,
    /// Bookkeeping the human never needs to see (the status tool,
    /// skill loading).
    Quiet,
}

impl Kind {
    /// Whether the call is a numbered work item: something the work
    /// pane shows as its own card and the words pane marks.
    pub fn numbered(self) -> bool {
        matches!(self, Kind::Edit | Kind::Check | Kind::Run | Kind::Worker)
    }

    /// The stage this kind of call reaches.
    pub fn stage(self) -> Stage {
        match self {
            Kind::Edit | Kind::Worker => Stage::Change,
            Kind::Check => Stage::Verify,
            Kind::Explore | Kind::Memory | Kind::Run => Stage::Understand,
            Kind::Ask | Kind::Quiet => Stage::None,
        }
    }
}

/// The turn's stages, in order. `Settled` is the turn's end and is
/// decided by the view (a turn settles when it stops running), never
/// by a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    None,
    Understand,
    Change,
    Verify,
}

impl Stage {
    /// The seam's verb for the stage while a turn runs.
    pub fn verb(self) -> &'static str {
        match self {
            Stage::None => "thinking",
            Stage::Understand => "reading",
            Stage::Change => "working",
            Stage::Verify => "checking",
        }
    }

    /// How much of the ring the stage fills, 0 to 1.
    pub fn fill(self) -> f32 {
        match self {
            Stage::None => 0.06,
            Stage::Understand => 0.22,
            Stage::Change => 0.47,
            Stage::Verify => 0.72,
        }
    }
}

fn field(arguments: &str, key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    value.get(key)?.as_str().map(str::to_string)
}

/// The path a file tool names, if any.
pub fn path_of(arguments: &str) -> Option<String> {
    field(arguments, "path")
}

/// The command a bash call runs, if any.
pub fn command_of(arguments: &str) -> Option<String> {
    field(arguments, "command")
}

/// Classifies one call. `verify` is the project's configured check
/// command: a bash call running it is a check whatever its words.
pub fn kind_of(name: &str, arguments: &str, verify: Option<&str>) -> Kind {
    if name.starts_with("agent_") {
        return Kind::Worker;
    }
    match name {
        // Questions for the human, in the conversation: the model's
        // asks and the full-access confirm.
        "ask_user" | "mode_confirm" => Kind::Ask,
        // The settle's proposal is memory, answered as a seal.
        "memory_proposal" => Kind::Memory,
        "edit_file" | "write_file" => Kind::Edit,
        "search_memory" | "memory" | "remember" => Kind::Memory,
        "read_file" => match path_of(arguments) {
            Some(path)
                if path.contains(".gertie/memory/") || path.starts_with(".gertie/memory") =>
            {
                Kind::Memory
            }
            _ => Kind::Explore,
        },
        "bash" => {
            let command = command_of(arguments).unwrap_or_default();
            if looks_like_check(&command, verify) {
                Kind::Check
            } else {
                Kind::Run
            }
        }
        "status" | "load_skill" => Kind::Quiet,
        _ => Kind::Explore,
    }
}

/// Whether a shell command verifies the work: the project's own check,
/// or a test, lint, type or build command by its words.
pub fn looks_like_check(command: &str, verify: Option<&str>) -> bool {
    if let Some(verify) = verify.map(str::trim).filter(|verify| !verify.is_empty())
        && command.contains(verify)
    {
        return true;
    }
    const WORDS: &[&str] = &[
        "test", "tests", "check", "clippy", "lint", "pytest", "jest", "vitest", "tsc", "mypy",
        "ruff", "vet", "eslint",
    ];
    command
        .split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|')
        .map(|word| word.trim_start_matches("./"))
        .any(|word| {
            WORDS.contains(&word)
                || word.ends_with("/build.sh")
                || word == "build.sh"
                || word.starts_with("test:")
        })
}

/// Hands out thread marks: one per command or worker run, one per
/// edited file (a second edit to the same file joins the first's card).
#[derive(Default)]
pub struct Numbering {
    next: usize,
    files: Vec<(String, usize)>,
}

impl Numbering {
    pub fn next(&mut self, kind: Kind, path: Option<&str>) -> usize {
        if kind == Kind::Edit
            && let Some(path) = path
        {
            if let Some((_, number)) = self.files.iter().find(|(seen, _)| seen == path) {
                return *number;
            }
            self.next += 1;
            self.files.push((path.to_string(), self.next));
            return self.next;
        }
        self.next += 1;
        self.next
    }
}

/// One call in the span, as the panes see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The call's row in the transcript.
    pub index: usize,
    pub kind: Kind,
    /// Its thread mark, 1-based, for numbered kinds.
    pub number: Option<usize>,
}

/// The shape of one turn's span of calls.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    pub items: Vec<Item>,
}

impl Shape {
    /// Folds a span's calls, in transcript order, given as
    /// `(row index, tool name, arguments)`.
    pub fn of<'a>(
        calls: impl IntoIterator<Item = (usize, &'a str, &'a str)>,
        verify: Option<&str>,
    ) -> Self {
        let mut numbering = Numbering::default();
        let items = calls
            .into_iter()
            .map(|(index, name, arguments)| {
                let kind = kind_of(name, arguments, verify);
                let number = kind
                    .numbered()
                    .then(|| numbering.next(kind, path_of(arguments).as_deref()));
                Item {
                    index,
                    kind,
                    number,
                }
            })
            .collect();
        Shape { items }
    }

    /// The furthest stage any call reached.
    pub fn stage(&self) -> Stage {
        self.items
            .iter()
            .map(|item| item.kind.stage())
            .max()
            .unwrap_or(Stage::None)
    }

    /// Whether the turn did no work worth a pane: no numbered items.
    pub fn chat_only(&self) -> bool {
        self.items.iter().all(|item| item.number.is_none())
    }

    /// The numbered items, every call (a file's second edit too).
    pub fn work(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|item| item.number.is_some())
    }

    /// One item per thread mark: the first call of each work item.
    pub fn marks(&self) -> Vec<&Item> {
        let mut seen = Vec::new();
        self.work()
            .filter(|item| {
                let number = item.number.unwrap_or_default();
                if seen.contains(&number) {
                    false
                } else {
                    seen.push(number);
                    true
                }
            })
            .collect()
    }

    /// Every call that carries `number`, in order.
    pub fn group(&self, number: usize) -> Vec<&Item> {
        self.work()
            .filter(|item| item.number == Some(number))
            .collect()
    }

    /// The thread mark of the call at transcript row `index`.
    pub fn number_at(&self, index: usize) -> Option<usize> {
        self.items
            .iter()
            .find(|item| item.index == index)
            .and_then(|item| item.number)
    }

    /// The numbers of the work items whose rows fall in `range`: the
    /// marks an assistant paragraph carries for the calls after it.
    pub fn numbers_in(&self, range: std::ops::Range<usize>) -> Vec<usize> {
        let mut numbers: Vec<usize> = self
            .items
            .iter()
            .filter(|item| range.contains(&item.index))
            .filter_map(|item| item.number)
            .collect();
        numbers.dedup();
        numbers
    }
}

/// The first sentence of a paragraph, for an edit card's summary: up to
/// the first sentence end, else the first line, capped.
pub fn first_sentence(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```"))
        .unwrap_or_default();
    let line = line.trim_start_matches(['#', '-', '*', '>', ' ']);
    let mut end = line.len();
    for (at, c) in line.char_indices() {
        if matches!(c, '.' | '!' | '?' | ':')
            && line[at + c.len_utf8()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
        {
            end = at + c.len_utf8();
            break;
        }
    }
    let sentence = line[..end].replace('`', "");
    const CAP: usize = 160;
    if sentence.chars().count() > CAP {
        let cut: String = sentence.chars().take(CAP).collect();
        format!("{}…", cut.trim_end())
    } else {
        sentence
    }
}

/// The density bar's blocks for a change: log-scaled so a one-line fix
/// reads as small and a rewrite as large, eight blocks at most, split
/// between additions and removals by their share, each side that is
/// non-zero getting at least one block.
pub fn density(adds: usize, removes: usize) -> (usize, usize) {
    let total = adds + removes;
    if total == 0 {
        return (0, 0);
    }
    let blocks = ((total as f32).log2().ceil() as usize + 1).clamp(1, 8);
    let mut add_blocks = ((adds as f32 / total as f32) * blocks as f32).round() as usize;
    if adds > 0 && add_blocks == 0 {
        add_blocks = 1;
    }
    if removes > 0 && add_blocks == blocks {
        add_blocks = blocks.saturating_sub(1).max(usize::from(adds > 0));
    }
    let remove_blocks = if removes > 0 {
        (blocks - add_blocks).max(1)
    } else {
        0
    };
    (add_blocks, remove_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_classify_by_purpose() {
        assert_eq!(
            kind_of("read_file", r#"{"path":"src/a.rs"}"#, None),
            Kind::Explore
        );
        assert_eq!(
            kind_of("read_file", r#"{"path":".gertie/memory/gotchas.md"}"#, None),
            Kind::Memory
        );
        assert_eq!(kind_of("edit_file", "{}", None), Kind::Edit);
        assert_eq!(kind_of("agent_review", "{}", None), Kind::Worker);
        assert_eq!(kind_of("ask_user", "{}", None), Kind::Ask);
        assert_eq!(kind_of("status", "{}", None), Kind::Quiet);
        assert_eq!(
            kind_of(
                "bash",
                r#"{"command":"cargo test -p gertie status_"}"#,
                None
            ),
            Kind::Check
        );
        assert_eq!(kind_of("bash", r#"{"command":"ls -la"}"#, None), Kind::Run);
        assert_eq!(
            kind_of(
                "bash",
                r#"{"command":"just verify-all"}"#,
                Some("just verify-all")
            ),
            Kind::Check
        );
        assert_eq!(
            kind_of("bash", r#"{"command":"./scripts/build.sh clippy"}"#, None),
            Kind::Check
        );
    }

    #[test]
    fn work_items_number_in_order_and_skip_looking() {
        let shape = Shape::of(
            [
                (3, "read_file", r#"{"path":"a.rs"}"#),
                (4, "edit_file", "{}"),
                (5, "grep", "{}"),
                (7, "bash", r#"{"command":"cargo test"}"#),
            ],
            None,
        );
        assert_eq!(shape.number_at(3), None);
        assert_eq!(shape.number_at(4), Some(1));
        assert_eq!(shape.number_at(7), Some(2));
        assert_eq!(shape.numbers_in(4..7), vec![1]);
        assert_eq!(shape.stage(), Stage::Verify);
        assert!(!shape.chat_only());
    }

    #[test]
    fn a_files_edits_share_one_mark() {
        let shape = Shape::of(
            [
                (1, "edit_file", r#"{"path":"a.rs"}"#),
                (2, "edit_file", r#"{"path":"b.rs"}"#),
                (3, "edit_file", r#"{"path":"a.rs"}"#),
                (4, "bash", r#"{"command":"cargo test"}"#),
            ],
            None,
        );
        assert_eq!(shape.number_at(1), Some(1));
        assert_eq!(shape.number_at(2), Some(2));
        assert_eq!(shape.number_at(3), Some(1), "a.rs again joins its card");
        assert_eq!(shape.number_at(4), Some(3));
        assert_eq!(shape.marks().len(), 3);
        assert_eq!(shape.group(1).len(), 2);
        assert_eq!(kind_of("mode_confirm", "{}", None), Kind::Ask);
        assert_eq!(kind_of("memory_proposal", "{}", None), Kind::Memory);
    }

    #[test]
    fn a_turn_that_only_looked_is_chat_only() {
        let shape = Shape::of([(1, "read_file", "{}"), (2, "search_memory", "{}")], None);
        assert!(shape.chat_only());
        assert_eq!(shape.stage(), Stage::Understand);
        assert_eq!(Shape::default().stage(), Stage::None);
    }

    #[test]
    fn the_first_sentence_stops_at_its_end() {
        assert_eq!(
            first_sentence("I moved the probe to `status.rs`. Then the tests."),
            "I moved the probe to status.rs."
        );
        assert_eq!(first_sentence("\n## Plan\nno period here"), "Plan");
        assert_eq!(first_sentence("version 1.2 is out"), "version 1.2 is out");
    }

    #[test]
    fn density_scales_and_keeps_both_sides() {
        assert_eq!(density(0, 0), (0, 0));
        assert_eq!(density(1, 0), (1, 0));
        let (adds, removes) = density(5, 18);
        assert!(adds >= 1 && removes > adds && adds + removes <= 8);
        let (adds, removes) = density(4000, 0);
        assert_eq!((adds, removes), (8, 0));
        let (adds, removes) = density(1, 1);
        assert_eq!((adds, removes), (1, 1));
    }
}
