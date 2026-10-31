//! A bounded line diff for the turn-diff view: longest-common-subsequence
//! over lines, with a compute guard for pathological inputs and a cap on
//! what a panel keeps in memory. Pure and UI-free so it can be tested
//! without a window.

/// One line of a diff: what it is decides its color.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Same(String),
    Add(String),
    Remove(String),
}

/// The diff of two texts: capped lines for display, uncapped counts for
/// the summary, and how many lines didn't fit.
#[derive(Clone, Debug, Default)]
pub struct Patch {
    pub lines: Vec<Line>,
    /// Total `+` lines across the whole diff, shown even when the
    /// display is capped.
    pub adds: usize,
    /// Total `−` lines across the whole diff.
    pub removes: usize,
    /// How many diff lines were dropped by the display cap.
    pub omitted: usize,
}

/// Cells the LCS table may use (each a `u32`): a million is 4 MB and
/// covers any file a review panel should open. Past it, the middle
/// falls back to remove-all-then-add-all: never minimal, always
/// bounded, still honest about what changed.
const MAX_CELLS: usize = 1_000_000;
/// Diff lines a panel keeps for display.
const MAX_LINES: usize = 400;

/// Splits into display lines without caring about a trailing newline.
fn split_lines(text: &str) -> Vec<&str> {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.lines().collect()
}

/// The diff of `before` → `after`.
pub fn lines(before: &str, after: &str) -> Patch {
    let before = split_lines(before);
    let after = split_lines(after);

    // Trim the common prefix and suffix: the LCS of what remains is
    // usually tiny, and identical texts answer without a table at all.
    let mut start = 0;
    while start < before.len() && start < after.len() && before[start] == after[start] {
        start += 1;
    }
    let mut end = 0;
    while end < before.len() - start
        && end < after.len() - start
        && before[before.len() - 1 - end] == after[after.len() - 1 - end]
    {
        end += 1;
    }
    let middle_before = &before[start..before.len() - end];
    let middle_after = &after[start..after.len() - end];

    // The middle as one op sequence: LCS order when the table fits,
    // remove-then-add when it doesn't.
    let ops: Vec<Line> = if middle_before.is_empty() {
        middle_after
            .iter()
            .map(|l| Line::Add(l.to_string()))
            .collect()
    } else if middle_after.is_empty() {
        middle_before
            .iter()
            .map(|l| Line::Remove(l.to_string()))
            .collect()
    } else if middle_before.len() * middle_after.len() <= MAX_CELLS {
        lcs_ops(middle_before, middle_after)
    } else {
        middle_before
            .iter()
            .map(|l| Line::Remove(l.to_string()))
            .chain(middle_after.iter().map(|l| Line::Add(l.to_string())))
            .collect()
    };

    let mut patch = Patch {
        adds: ops.iter().filter(|op| matches!(op, Line::Add(_))).count(),
        removes: ops
            .iter()
            .filter(|op| matches!(op, Line::Remove(_)))
            .count(),
        ..Patch::default()
    };
    // Assemble for display: common context, the middle, common context
    // again (capped). When the middle doesn't fit whole, the tail
    // context goes with it (it sits past the cut) and the counts of
    // what was dropped live in `omitted`.
    let mut display: Vec<Line> = (0..start)
        .map(|index| Line::Same(before[index].to_string()))
        .collect();
    let room = MAX_LINES.saturating_sub(display.len());
    let kept = room.min(ops.len());
    display.extend(ops[..kept].iter().cloned());
    patch.omitted = ops.len() - kept;
    if patch.omitted == 0 {
        display.extend(
            (0..end).map(|index| Line::Same(before[before.len() - end + index].to_string())),
        );
    }
    patch.lines = display;
    patch
}

/// LCS table over the middle, walked back into ops. Called only when
/// the cell count fits the budget.
fn lcs_ops(before: &[&str], after: &[&str]) -> Vec<Line> {
    let rows = before.len();
    let columns = after.len();
    let mut table = vec![0u32; (rows + 1) * (columns + 1)];
    for row in (0..rows).rev() {
        for column in (0..columns).rev() {
            table[row * (columns + 1) + column] = if before[row] == after[column] {
                table[(row + 1) * (columns + 1) + column + 1] + 1
            } else {
                table[(row + 1) * (columns + 1) + column]
                    .max(table[row * (columns + 1) + column + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let mut row = 0;
    let mut column = 0;
    while row < rows && column < columns {
        if before[row] == after[column] {
            ops.push(Line::Same(before[row].to_string()));
            row += 1;
            column += 1;
        } else if table[(row + 1) * (columns + 1) + column]
            >= table[row * (columns + 1) + column + 1]
        {
            ops.push(Line::Remove(before[row].to_string()));
            row += 1;
        } else {
            ops.push(Line::Add(after[column].to_string()));
            column += 1;
        }
    }
    ops.extend(before[row..].iter().map(|l| Line::Remove(l.to_string())));
    ops.extend(after[column..].iter().map(|l| Line::Add(l.to_string())));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(patch: &Patch) -> Vec<&'static str> {
        patch
            .lines
            .iter()
            .map(|line| match line {
                Line::Same(_) => " ",
                Line::Add(_) => "+",
                Line::Remove(_) => "-",
            })
            .collect()
    }

    #[test]
    fn identical_texts_diff_to_nothing() {
        let patch = lines("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(kinds(&patch), vec![" ", " ", " "]);
        assert_eq!((patch.adds, patch.removes), (0, 0));
        assert_eq!(patch.omitted, 0);
    }

    #[test]
    fn a_changed_line_reads_remove_then_add() {
        let patch = lines("a\nb\nc\n", "a\nx\nc\n");
        assert_eq!(kinds(&patch), vec![" ", "-", "+", " "]);
        let texts: Vec<&str> = patch
            .lines
            .iter()
            .map(|line| match line {
                Line::Same(text) | Line::Add(text) | Line::Remove(text) => text.as_str(),
            })
            .collect();
        assert_eq!(texts, vec!["a", "b", "x", "c"]);
        assert_eq!((patch.adds, patch.removes), (1, 1));
    }

    #[test]
    fn new_and_deleted_files_are_all_one_side() {
        let added = lines("", "one\ntwo\n");
        assert_eq!(kinds(&added), vec!["+", "+"]);
        let removed = lines("one\ntwo\n", "");
        assert_eq!(kinds(&removed), vec!["-", "-"]);
        // A trailing newline is not a line of its own; a fully empty
        // text is zero lines either way.
        let empty = lines("", "");
        assert!(empty.lines.is_empty());
        let empty = lines("\n", "\n");
        assert!(empty.lines.is_empty());
    }

    #[test]
    fn the_display_cap_drops_nothing_from_the_counts() {
        let before: String = (0..600).map(|i| format!("line {i}\n")).collect();
        let after: String = (0..600).map(|i| format!("line {i} v2\n")).collect();
        let patch = lines(&before, &after);
        assert_eq!(patch.lines.len(), super::MAX_LINES);
        assert_eq!(patch.omitted, 800);
        assert_eq!((patch.adds, patch.removes), (600, 600));
    }

    #[test]
    fn pathological_inputs_fall_back_to_remove_then_add() {
        // 1001 × 1001 middle cells bust the table budget; the patch
        // stays complete and bounded anyway.
        let before: String = (0..1001).map(|i| format!("old {i}\n")).collect();
        let after: String = (0..1001).map(|i| format!("new {i}\n")).collect();
        let patch = lines(&before, &after);
        assert_eq!((patch.adds, patch.removes), (1001, 1001));
        assert_eq!(patch.lines.len(), super::MAX_LINES);
        assert_eq!(patch.omitted, 1001 + 1001 - super::MAX_LINES);
    }
}
