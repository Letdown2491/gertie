//! Result shaping: rtk's insight, domesticated. Tool output competes for
//! the model's context, and most dev commands produce mostly noise —
//! passing tests, unchanged files, progress bars. A shaper rewrites a
//! tool result to what the model needs, per command shape.
//!
//! Invariants (rtk's design principles, adopted as gertie-core rules):
//! 1. Fail-open: any doubt returns the original output unchanged.
//! 2. Exit status and error text are never dropped — only de-emphasized.
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
    let head = command
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    let looks_like_tests = [
        "cargo test",
        "cargo nextest",
        "go test",
        "pytest",
        "npm test",
        "vitest",
    ]
    .iter()
    .any(|marker| head.starts_with(marker));
    if looks_like_tests {
        return Shape::Failures;
    }
    let looks_like_build = [
        "cargo build",
        "cargo check",
        "cargo clippy",
        "make",
        "tsc",
        "npm run build",
    ]
    .iter()
    .any(|marker| head.starts_with(marker));
    if looks_like_build {
        return Shape::Errors;
    }
    Shape::Pass
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
        Shape::Pass => return raw.to_string(),
    };
    // Fail-open: an overzealous transform that dropped everything is
    // worse than noise — fall back to the truth.
    if shaped.trim().is_empty() {
        raw.to_string()
    } else {
        shaped
    }
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

/// Build tools: error/warning lines plus the tail (final status, e.g.
/// `error: could not compile …`), chatter dropped.
fn shape_errors(raw: &str) -> String {
    let kept: Vec<String> = raw
        .lines()
        .filter(|line| is_failure_line(line))
        .map(String::from)
        .collect();
    if kept.is_empty() {
        // No diagnostics matched: the tail is all we can say.
        let start = raw.lines().count().saturating_sub(5);
        return raw.lines().skip(start).collect::<Vec<_>>().join("\n");
    }
    let mut out = kept;
    // Keep the command's own verdict (usually the last non-empty line) if
    // it wasn't already an error/warning line.
    if let Some(last) = raw.lines().rev().find(|line| !line.trim().is_empty())
        && !out.iter().any(|kept| kept == last)
    {
        out.push(last.to_string());
    }
    out.join("\n")
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

    #[test]
    fn shapes_fail_open() {
        // A classification that would drop everything returns the input.
        assert_eq!(shape("cargo test", "hello\n"), "hello");
        assert_eq!(shape("cargo build", ""), "");
    }
}
