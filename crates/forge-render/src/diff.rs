//! Diff rendering.
//!
//! Git produces unified diffs; this turns them into something a browser can
//! display, and computes word-level changes within a line so a one-character
//! edit does not look like a whole line rewritten.

use std::time::Duration;

use similar::{Algorithm, ChangeTag, TextDiff};

/// How long the differ may spend before returning its best effort.
///
/// A public endpoint cannot offer unbounded computation on user-supplied
/// content, and a slightly worse diff is a much better outcome than a request
/// that never returns.
const BUDGET: Duration = Duration::from_millis(500);

/// Number of unchanged lines shown around each change.
pub const CONTEXT: usize = 3;

/// Above this many changed lines, word-level refinement is skipped.
const MAX_INLINE_LINES: usize = 2_000;

/// One line of a rendered diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number on the left, absent for insertions.
    pub old_line: Option<usize>,
    /// Line number on the right, absent for deletions.
    pub new_line: Option<usize>,
    /// The line, split into runs. Emphasized runs are the parts that actually
    /// changed within the line.
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// A span of text within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    /// True when this run is part of what changed, for intra-line highlighting.
    pub emphasized: bool,
}

/// A contiguous group of changes with its surrounding context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub lines: Vec<DiffLine>,
}

/// A rendered diff of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    pub added: usize,
    pub removed: usize,
}

impl FileDiff {
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Whether this diff is large enough that a UI should collapse it.
    pub fn is_large(&self) -> bool {
        self.added + self.removed > 400
    }
}

/// Diff two versions of a file.
pub fn diff_text(old: &str, new: &str) -> FileDiff {
    let diff = TextDiff::configure()
        // Patience produces hunks that line up with how people think about
        // their edits, rather than the shortest edit script.
        .algorithm(Algorithm::Patience)
        .timeout(BUDGET)
        .diff_lines(old, new);

    let changed = diff
        .iter_all_changes()
        .filter(|c| c.tag() != ChangeTag::Equal)
        .count();
    let refine_inline = changed <= MAX_INLINE_LINES;

    let mut hunks = Vec::new();
    let mut added = 0;
    let mut removed = 0;

    for group in diff.grouped_ops(CONTEXT) {
        let mut lines = Vec::new();
        for op in &group {
            if refine_inline {
                for change in diff.iter_inline_changes(op) {
                    let kind = tag_to_kind(change.tag());
                    match kind {
                        LineKind::Added => added += 1,
                        LineKind::Removed => removed += 1,
                        LineKind::Context => {}
                    }
                    lines.push(DiffLine {
                        kind,
                        old_line: change.old_index().map(|i| i + 1),
                        new_line: change.new_index().map(|i| i + 1),
                        runs: change
                            .iter_strings_lossy()
                            .map(|(emphasized, text)| Run {
                                text: text.trim_end_matches('\n').to_string(),
                                emphasized,
                            })
                            .collect(),
                    });
                }
            } else {
                for change in diff.iter_changes(op) {
                    let kind = tag_to_kind(change.tag());
                    match kind {
                        LineKind::Added => added += 1,
                        LineKind::Removed => removed += 1,
                        LineKind::Context => {}
                    }
                    lines.push(DiffLine {
                        kind,
                        old_line: change.old_index().map(|i| i + 1),
                        new_line: change.new_index().map(|i| i + 1),
                        runs: vec![Run {
                            text: change.value().trim_end_matches('\n').to_string(),
                            emphasized: false,
                        }],
                    });
                }
            }
        }
        if !lines.is_empty() {
            hunks.push(Hunk { lines });
        }
    }

    FileDiff {
        hunks,
        added,
        removed,
    }
}

fn tag_to_kind(tag: ChangeTag) -> LineKind {
    match tag {
        ChangeTag::Equal => LineKind::Context,
        ChangeTag::Insert => LineKind::Added,
        ChangeTag::Delete => LineKind::Removed,
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn text_of(line: &DiffLine) -> String {
        line.runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn an_added_line_is_reported_as_added() {
        let d = diff_text("one\n", "one\ntwo\n");
        check!(d.added == 1);
        check!(d.removed == 0);

        let added: Vec<String> = d.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .map(text_of)
            .collect();
        check!(added == vec!["two"]);
    }

    #[test]
    fn a_removed_line_is_reported_as_removed() {
        let d = diff_text("one\ntwo\n", "one\n");
        check!(d.removed == 1);
        check!(d.added == 0);
    }

    #[test]
    fn line_numbers_are_one_based_on_the_side_they_exist() {
        let d = diff_text("a\nb\n", "a\nB\n");
        let removed = d.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Removed)
            .unwrap();
        let added = d.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Added)
            .unwrap();

        check!(removed.old_line == Some(2));
        check!(
            removed.new_line.is_none(),
            "a removed line has no new number"
        );
        check!(added.new_line == Some(2));
        check!(added.old_line.is_none());
    }

    #[test]
    fn a_small_edit_is_narrowed_to_the_changed_words() {
        // The point of inline refinement: without it, changing one word marks
        // the entire line as rewritten and the reader has to find the change.
        let d = diff_text("The quick brown fox jumps\n", "The quick red fox jumps\n");
        let added = d
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .find(|l| l.kind == LineKind::Added)
            .unwrap();

        let emphasized: String = added
            .runs
            .iter()
            .filter(|r| r.emphasized)
            .map(|r| r.text.as_str())
            .collect();
        check!(emphasized.contains("red"), "runs: {:?}", added.runs);
        check!(
            !emphasized.contains("quick"),
            "unchanged words must not be marked"
        );
    }

    #[test]
    fn identical_files_produce_no_hunks() {
        let d = diff_text("same\n", "same\n");
        check!(d.is_empty());
        check!(d.added == 0 && d.removed == 0);
    }

    #[test]
    fn context_lines_surround_a_change_without_being_counted() {
        let old = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        let new = old.replace("line 10\n", "line ten\n");
        let d = diff_text(&old, &new);

        check!(d.added == 1 && d.removed == 1);
        let context = d.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Context)
            .count();
        check!(context > 0 && context <= CONTEXT * 2);
        // Unchanged regions far from the edit are omitted entirely.
        check!(d.hunks[0].lines.len() < 20);
    }

    #[test]
    fn a_file_created_from_nothing_is_all_additions() {
        let d = diff_text("", "new\nfile\n");
        check!(d.added == 2 && d.removed == 0);
    }

    #[test]
    fn a_large_diff_is_flagged_for_collapsing() {
        let new = (0..500).map(|i| format!("added {i}\n")).collect::<String>();
        let d = diff_text("", &new);
        check!(d.is_large());
    }

    #[test]
    fn a_diff_beyond_the_inline_budget_still_renders() {
        // Word-level refinement is dropped, but the diff itself must survive.
        let new = (0..MAX_INLINE_LINES + 100)
            .map(|i| format!("line {i}\n"))
            .collect::<String>();
        let d = diff_text("", &new);
        check!(d.added == MAX_INLINE_LINES + 100);
        check!(!d.is_empty());
    }

    #[test]
    fn files_without_trailing_newlines_are_handled() {
        let d = diff_text("no newline", "no newline at all");
        check!(!d.is_empty());
    }
}
