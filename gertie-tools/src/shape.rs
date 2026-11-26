//! Result shaping: rtk's insight, domesticated. Tool output competes for
//! the model's context, and most dev commands produce mostly noise,
//! passing tests, unchanged files, progress bars. A shaper rewrites a
//! tool result to what the model needs, per command shape.
//!
//! Invariants (rtk's design principles, adopted as gertie-core rules):
//! 1. Fail-open: any doubt returns the original output unchanged.
//! 2. Exit status and error text are never dropped, only de-emphasized.
//! 3. Shape once, at execution time: the shaped string is what enters
//!    history and the session DB, so the prompt cache and reloads agree.
//!
//! v1 covers the two loudest command families (test runners, builds).
//! The taxonomy is extensible; boring detection, boring transforms.

/// What a command's output is mostly made of, guessed from the command.
enum Shape {
    /// Test runners: passing tests collapse to a count, failures stay.
    Failures,
    /// Build/compile tools: errors and warnings stay, chatter collapses.
    Errors,
    /// Nothing confident to say: pass through (size-capped downstream).
    Pass,
}

fn classify(command: &str) -> Shape {
    // A filter at the end means the model already chose what to see.
    let piped = command.split("||").any(|part| part.contains('|'));
    if piped {
        return Shape::Pass;
    }
    // Commands arrive wrapped (`cd x && cargo test`, `A=1 make check`):
    // the last segment that names a known shape decides.
    command
        .rsplit(['&', ';'])
        .find_map(classify_segment)
        .unwrap_or(Shape::Pass)
}

/// One simple command: leading env assignments skipped, then the
/// program and its first words.
fn classify_segment(segment: &str) -> Option<Shape> {
    let words: Vec<&str> = segment
        .split_whitespace()
        .skip_while(|word| {
            word.split_once('=')
                .is_some_and(|(name, _)| !name.is_empty() && !name.contains('/'))
        })
        .take(3)
        .collect();
    let head = words.join(" ");
    let tests = [
        "cargo test",
        "cargo nextest",
        "go test",
        "pytest",
        "npm test",
        "vitest",
    ];
    if tests.iter().any(|marker| head.starts_with(marker)) {
        return Some(Shape::Failures);
    }
    let builds = [
        "cargo build",
        "cargo check",
        "cargo clippy",
        "tsc",
        "npm run build",
    ];
    if builds.iter().any(|marker| head.starts_with(marker)) {
        return Some(Shape::Errors);
    }
    // Task runners and project scripts: the verb names the shape
    // (`make test`, `./scripts/build.sh check`).
    let program = words.first()?;
    let runner = ["make", "just", "npm", "pnpm", "yarn"].contains(program)
        || program.contains('/')
        || program.ends_with(".sh");
    if !runner {
        return None;
    }
    let verbs = &words[1..];
    if verbs.iter().any(|verb| matches!(*verb, "test" | "tests")) {
        Some(Shape::Failures)
    } else if verbs
        .iter()
        .any(|verb| matches!(*verb, "build" | "check" | "clippy" | "lint"))
        || *program == "make"
    {
        Some(Shape::Errors)
    } else {
        None
    }
}

/// True for lines that carry failure information in any of the tool
/// families we shape.
fn is_failure_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("error")
        || lower.contains("fail")
        || lower.contains("panicked")
        || lower.contains("warning")
        || lower.contains("assertion")
}

/// Shapes tool output by command shape. `raw` is what the command wrote;
/// the result is what the model sees.
pub fn shape(command: &str, raw: &str) -> String {
    if raw.is_empty() {
        return raw.to_string();
    }
    let shaped = match classify(command) {
        Shape::Failures => shape_failures(raw),
        Shape::Errors => shape_errors(raw),
        Shape::Pass => raw.to_string(),
    };
    // Fail-open: an overzealous transform that dropped everything is
    // worse than noise, fall back to the truth.
    let shaped = if shaped.trim().is_empty() {
        raw.to_string()
    } else {
        shaped
    };
    elide_middle(shaped)
}

/// Lines kept from the start and the end of long output.
const HEAD_LINES: usize = 40;
const TAIL_LINES: usize = 60;
/// Failure lines kept from the elided middle.
const MIDDLE_FAILURES: usize = 40;

/// Long output keeps its head (what ran), every failure line of the
/// middle up to a cap, and its tail (the verdict, where errors pile
/// up). The cap downstream used to keep only the first 16k chars,
/// cutting exactly the part that mattered. Short output passes.
fn elide_middle(text: String) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= HEAD_LINES + TAIL_LINES + MIDDLE_FAILURES {
        return text;
    }
    let middle = &lines[HEAD_LINES..lines.len() - TAIL_LINES];
    let mut out: Vec<String> = lines[..HEAD_LINES]
        .iter()
        .map(|line| line.to_string())
        .collect();
    let mut elided = 0usize;
    let mut kept = 0usize;
    let flush = |out: &mut Vec<String>, elided: &mut usize| {
        if *elided > 0 {
            out.push(format!("… [{elided} lines elided]"));
            *elided = 0;
        }
    };
    for line in middle {
        if kept < MIDDLE_FAILURES && is_failure_line(line) {
            flush(&mut out, &mut elided);
            out.push(line.to_string());
            kept += 1;
        } else {
            elided += 1;
        }
    }
    flush(&mut out, &mut elided);
    out.extend(
        lines[lines.len() - TAIL_LINES..]
            .iter()
            .map(|line| line.to_string()),
    );
    let total = lines.len() - out.len();
    out.push(format!(
        "… [{total} lines elided in all; rerun piped through grep or tail to see a part]"
    ));
    out.join("\n")
}

/// Test runners: `test foo::bar ... ok` lines collapse to a count;
/// failures, panics, and summary lines stay verbatim.
fn shape_failures(raw: &str) -> String {
    let mut passing = 0usize;
    let mut kept: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let is_passing = {
            let trimmed = line.trim();
            trimmed.starts_with("test ") && trimmed.ends_with(" ok")
        };
        if is_passing {
            passing += 1;
        } else {
            kept.push(line);
        }
    }

    let mut out: Vec<String> = Vec::new();
    for line in kept {
        // Collapsed runs of interleaved noise keep the summary readable;
        // blank-line runs shrink to one.
        if line.trim().is_empty() && out.last().map(|last| last.trim().is_empty()) == Some(true) {
            continue;
        }
        out.push(line.to_string());
    }
    if passing > 0 {
        out.push(format!("… {passing} passing tests (collapsed)"));
    }
    out.join("\n")
}

/// Build tools: each diagnostic with its block (the `-->` location,
/// the source excerpt, the notes) plus the tail (final status, e.g.
/// `error: could not compile …`), progress chatter dropped.
fn shape_errors(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_block = 0usize;
    for line in raw.lines() {
        if is_failure_line(line) {
            out.push(line.to_string());
            in_block = 1;
            continue;
        }
        let trimmed = line.trim_start();
        let continues = in_block > 0
            && in_block < BLOCK_LINES
            && !trimmed.is_empty()
            && !is_progress_line(trimmed)
            && (line.starts_with(char::is_whitespace)
                || trimmed.starts_with("-->")
                || trimmed.starts_with('=')
                || trimmed
                    .split_once(" |")
                    .is_some_and(|(number, _)| number.chars().all(|c| c.is_ascii_digit())));
        if continues {
            out.push(line.to_string());
            in_block += 1;
        } else {
            in_block = 0;
        }
    }
    if out.is_empty() {
        // No diagnostics matched: the tail is all we can say.
        let start = raw.lines().count().saturating_sub(5);
        return raw.lines().skip(start).collect::<Vec<_>>().join("\n");
    }
    // Keep the command's own verdict (usually the last non-empty line) if
    // it wasn't already an error/warning line.
    if let Some(last) = raw.lines().rev().find(|line| !line.trim().is_empty())
        && !out.iter().any(|kept| kept == last)
    {
        out.push(last.to_string());
    }
    out.join("\n")
}

/// Lines a diagnostic block may run to, its header included.
const BLOCK_LINES: usize = 16;

/// Build progress, never part of a diagnostic even when indented.
fn is_progress_line(trimmed: &str) -> bool {
    [
        "Compiling ",
        "Checking ",
        "Finished ",
        "Downloading ",
        "Downloaded ",
        "Building ",
        "Running ",
        "Fresh ",
    ]
    .iter()
    .any(|marker| trimmed.starts_with(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_tests_collapse_and_failures_stay() {
        let raw = "running 3 tests\n\
                   test a ... ok\n\
                   test b ... ok\n\
                   test c ... FAILED\n\
                   failures:\n\
                   \n\
                   failures:\n    c\n\
                   test result: FAILED. 2 passed; 1 failed;\n";
        let shaped = shape("cargo test", raw);

        assert!(shaped.contains("2 passing tests (collapsed)"));
        assert!(shaped.contains("test c ... FAILED"));
        assert!(shaped.contains("test result: FAILED"));
        assert!(!shaped.contains("test a ... ok"));
    }

    #[test]
    fn build_output_keeps_diagnostics_and_the_verdict() {
        let raw = "   Compiling gertie v0.1.0\n\
                   warning: unused variable\n  --> src/x.rs\n\
                   error[E0308]: mismatched types\n  --> src/y.rs\n\
                   error: could not compile `gertie` (bin \"gertie\")\n";
        let shaped = shape("cargo build", raw);

        assert!(shaped.contains("error[E0308]"));
        assert!(shaped.contains("could not compile"));
        assert!(shaped.contains("warning: unused"));
        assert!(!shaped.contains("Compiling gertie"));
    }

    #[test]
    fn clean_builds_fall_back_to_the_tail() {
        let raw = "   Compiling a\n   Compiling b\n   Finished dev in 3s\n";
        let shaped = shape("cargo build", raw);
        assert!(shaped.contains("Finished dev in 3s"));
    }

    #[test]
    fn unclassified_commands_pass_through() {
        let raw = "total 4\ndrwxr-xr-x 2 me me 4096 .\n";
        assert_eq!(shape("ls -la", raw), raw);
    }

    /// The location is half the diagnostic: `error[E0308]` without its
    /// `--> src/y.rs:12:5` sends the model hunting.
    #[test]
    fn build_diagnostics_keep_their_location_and_context() {
        let raw = "   Compiling gertie v0.1.0\n\
                   error[E0308]: mismatched types\n\
                   \x20 --> src/y.rs:12:5\n\
                   \x20  |\n\
                   12 |     let x: u32 = \"no\";\n\
                   \x20  |                  ^^^^ expected `u32`, found `&str`\n\
                   \n\
                   \x20  Compiling other v0.1.0\n\
                   error: could not compile `gertie`\n";
        let shaped = shape("cargo build", raw);
        assert!(shaped.contains("--> src/y.rs:12:5"), "{shaped}");
        assert!(shaped.contains("expected `u32`"), "{shaped}");
        assert!(!shaped.contains("Compiling"), "{shaped}");
    }

    /// Commands arrive wrapped: a cd first, a redirect, a project script.
    #[test]
    fn wrapped_commands_still_classify() {
        let raw = "running 2 tests\ntest a ... ok\ntest b ... ok\ntest result: ok. 2 passed\n";
        for command in [
            "cd gertie && cargo test",
            "cargo test --workspace 2>&1",
            "RUST_LOG=debug cargo test",
            "./scripts/build.sh test",
        ] {
            assert!(
                shape(command, raw).contains("2 passing tests (collapsed)"),
                "{command}"
            );
        }
        // Piped through a filter: the model already chose what to see.
        assert_eq!(shape("cargo test 2>&1 | tail -3", raw), raw);
    }

    /// Long output nobody classified keeps its head, every failure line
    /// in the middle, and its tail, where the verdict and the errors
    /// usually are; the cut says how much went and how to get it.
    #[test]
    fn long_unclassified_output_keeps_head_failures_and_tail() {
        let mut raw = String::new();
        for index in 0..1000 {
            raw.push_str(&format!("line {index}\n"));
            if index == 500 {
                raw.push_str("ERROR: disk quota exceeded at step 500\n");
            }
        }
        raw.push_str("final verdict: 1 problem\n");
        let shaped = shape("./deploy.sh", &raw);
        assert!(shaped.contains("line 0\n"), "head");
        assert!(shaped.contains("disk quota exceeded"), "the middle failure");
        assert!(shaped.contains("final verdict"), "tail");
        assert!(!shaped.contains("line 300\n"), "the middle chatter goes");
        assert!(shaped.contains("lines elided"), "{shaped}");
        assert!(
            shaped.len() < raw.len() / 4,
            "{} vs {}",
            shaped.len(),
            raw.len()
        );
        // Short output is untouched.
        assert_eq!(shape("./deploy.sh", "a\nb\n"), "a\nb\n");
    }

    #[test]
    fn shapes_fail_open() {
        // A classification that would drop everything returns the input.
        assert_eq!(shape("cargo test", "hello\n"), "hello");
        assert_eq!(shape("cargo build", ""), "");
    }
}
