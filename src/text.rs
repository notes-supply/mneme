use std::collections::HashMap;

use similar::{Algorithm, ChangeTag, DiffTag, TextDiff};

const CONTEXT_LINES: usize = 2;
/// Above this many words on one side, a changed block is compared by word counts instead of
/// word order. An ordered diff of such a block can take more memory than the reader has.
const ORDERED_WORDS: u32 = 20_000;
/// Above this many lines on one side, two texts are compared by word counts, and a diff is not
/// shown line by line. A line diff of such texts can take more memory than the reader has.
const ORDERED_LINES: usize = 20_000;

/// A word is a run of non-whitespace characters.
#[must_use]
pub fn word_count(text: &str) -> u32 {
    text.split_whitespace()
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Churn {
    pub added: u32,
    pub removed: u32,
}

/// Words added and removed: a line diff, then a word diff inside each changed block.
/// The word diff is similar's heuristic Myers, with no time limit, so the counts are the
/// same on every run. A very long block or text is compared by word counts.
#[must_use]
pub fn churn(old: &str, new: &str) -> Churn {
    if too_many_lines(old, new) {
        return counted(old, new);
    }
    let lines = TextDiff::configure()
        .algorithm(Algorithm::Histogram)
        .diff_lines(old, new);
    let mut churn = Churn::default();
    for op in lines.ops() {
        let old_block =
            || -> String { op.old_range().filter_map(|i| lines.old_slice(i)).collect() };
        let new_block =
            || -> String { op.new_range().filter_map(|i| lines.new_slice(i)).collect() };
        match op.tag() {
            DiffTag::Equal => {}
            DiffTag::Insert => churn.added += word_count(&new_block()),
            DiffTag::Delete => churn.removed += word_count(&old_block()),
            DiffTag::Replace => {
                let (old_block, new_block) = (old_block(), new_block());
                if word_count(&old_block).max(word_count(&new_block)) > ORDERED_WORDS {
                    let counted = counted(&old_block, &new_block);
                    churn.added += counted.added;
                    churn.removed += counted.removed;
                    continue;
                }
                let words = TextDiff::configure().diff_words(&old_block, &new_block);
                for change in words.iter_all_changes() {
                    if change.value().trim().is_empty() {
                        continue;
                    }
                    match change.tag() {
                        ChangeTag::Insert => churn.added += 1,
                        ChangeTag::Delete => churn.removed += 1,
                        ChangeTag::Equal => {}
                    }
                }
            }
        }
    }
    churn
}

/// Words added and removed when only the number of each word counts.
fn counted(old: &str, new: &str) -> Churn {
    let mut balance: HashMap<&str, i64> = HashMap::new();
    for word in old.split_whitespace() {
        *balance.entry(word).or_default() -= 1;
    }
    for word in new.split_whitespace() {
        *balance.entry(word).or_default() += 1;
    }
    let total = |sign: i64| -> u32 {
        let sum: i64 = balance.values().map(|n| (n * sign).max(0)).sum();
        sum.try_into().unwrap_or(u32::MAX)
    };
    Churn {
        added: total(1),
        removed: total(-1),
    }
}

fn too_many_lines(old: &str, new: &str) -> bool {
    line_breaks(old).max(line_breaks(new)) > ORDERED_LINES
}

/// Line ends as similar and comrak see them: LF, CR and CRLF.
fn line_breaks(text: &str) -> usize {
    let bytes = text.as_bytes();
    (0..bytes.len())
        .filter(|&i| bytes[i] == b'\n' || (bytes[i] == b'\r' && bytes.get(i + 1) != Some(&b'\n')))
        .count()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub changed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DiffLine {
    Same(String),
    Removed(Vec<Span>),
    Added(Vec<Span>),
    /// Unchanged lines left out between two groups of changes, or at either end.
    Skipped(usize),
}

/// Changed lines with a little context. Changed words inside a changed line are marked.
/// `None` when the texts are too long to show line by line.
#[must_use]
pub fn diff(old: &str, new: &str) -> Option<Vec<DiffLine>> {
    if too_many_lines(old, new) {
        return None;
    }
    let lines = TextDiff::configure()
        .algorithm(Algorithm::Histogram)
        .diff_lines(old, new);
    let mut out = Vec::new();
    let mut shown_until = 0;
    for group in lines.grouped_ops(CONTEXT_LINES) {
        let start = group.first().map_or(0, |op| op.old_range().start);
        if start > shown_until {
            out.push(DiffLine::Skipped(start - shown_until));
        }
        for op in &group {
            let long = op.tag() == DiffTag::Replace && {
                let old: u32 = op
                    .old_range()
                    .filter_map(|i| lines.old_slice(i))
                    .map(word_count)
                    .sum();
                let new: u32 = op
                    .new_range()
                    .filter_map(|i| lines.new_slice(i))
                    .map(word_count)
                    .sum();
                old.max(new) > ORDERED_WORDS
            };
            let changes: Vec<_> = if long {
                lines.iter_changes(op).map(Into::into).collect()
            } else {
                lines.iter_inline_changes_deadline(op, None).collect()
            };
            for change in changes {
                let spans = change
                    .iter_strings_lossy()
                    .map(|(changed, text)| Span {
                        text: text.trim_end_matches('\n').to_owned(),
                        changed,
                    })
                    .filter(|span| !span.text.is_empty())
                    .collect::<Vec<_>>();
                out.push(match change.tag() {
                    ChangeTag::Equal => {
                        DiffLine::Same(spans.into_iter().map(|span| span.text).collect())
                    }
                    ChangeTag::Delete => DiffLine::Removed(spans),
                    ChangeTag::Insert => DiffLine::Added(spans),
                });
            }
        }
        shown_until = group.last().map_or(0, |op| op.old_range().end);
    }
    if lines.old_len() > shown_until && !out.is_empty() {
        out.push(DiffLine::Skipped(lines.old_len() - shown_until));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_runs_of_non_whitespace() {
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("  # Gotländska  minnen\n\n- a, b.\n"), 6);
    }

    #[test]
    fn churn_counts_changed_words_inside_a_long_line() {
        let old = "# T\n\nOne long paragraph with many words that stays the same.\n";
        let new = "# T\n\nOne long paragraph with several words that still stays the same.\n";
        assert_eq!(
            churn(old, new),
            Churn {
                added: 2,
                removed: 1
            }
        );
    }

    #[test]
    fn churn_counts_whole_added_and_removed_lines() {
        assert_eq!(
            churn("a b\nc\n", "a b\nc\nd e f\n"),
            Churn {
                added: 3,
                removed: 0
            }
        );
        assert_eq!(
            churn("a b\nc\n", "c\n"),
            Churn {
                added: 0,
                removed: 2
            }
        );
        assert_eq!(churn("", "x y\n").added, 2);
        assert_eq!(churn("same\n", "same\n"), Churn::default());
    }

    #[test]
    fn whitespace_only_changes_have_no_word_churn() {
        assert_eq!(churn("a  b\n", "a b\n"), Churn::default());
    }

    #[test]
    fn one_changed_word_in_a_very_long_paragraph_counts_as_one() {
        let old = (0..30_000)
            .map(|i| format!("w{i} "))
            .collect::<Vec<_>>()
            .concat();
        let new = old.replacen("w7 ", "x7 ", 1);
        assert_eq!(
            churn(&old, &new),
            Churn {
                added: 1,
                removed: 1
            }
        );
    }

    #[test]
    fn diff_marks_changed_words_and_skips_far_context() {
        let old = "1\n2\n3\n4\n5\n6\nkeep this old word\n7\n8\n9\n10\n";
        let new = "1\n2\n3\n4\n5\n6\nkeep this new word\n7\n8\n9\n10\n";
        let lines = diff(old, new).unwrap();
        assert_eq!(lines.first(), Some(&DiffLine::Skipped(4)));
        assert_eq!(lines.last(), Some(&DiffLine::Skipped(2)));
        let removed = lines
            .iter()
            .find_map(|line| match line {
                DiffLine::Removed(spans) => Some(spans),
                _ => None,
            })
            .unwrap();
        let marked: Vec<&str> = removed
            .iter()
            .filter(|s| s.changed)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(marked, ["old"]);
        assert!(lines.contains(&DiffLine::Same("6".to_owned())));
    }

    #[test]
    fn identical_text_has_no_diff_lines() {
        assert_eq!(diff("a\nb\n", "a\nb\n"), Some(Vec::new()));
    }

    #[test]
    fn a_long_block_is_compared_by_word_counts() {
        let old = "x ".repeat(30_000);
        let new = "y ".repeat(30_000);
        assert_eq!(
            churn(&old, &new),
            Churn {
                added: 30_000,
                removed: 30_000
            }
        );
        let lines = diff(&old, &new).unwrap();
        assert!(
            lines.iter().all(|line| match line {
                DiffLine::Removed(spans) | DiffLine::Added(spans) =>
                    spans.iter().all(|s| !s.changed),
                _ => true,
            }),
            "no inline marks on a block this long"
        );
    }

    #[test]
    fn very_many_lines_are_compared_by_word_counts_and_not_shown() {
        let old = "x\n".repeat(30_000);
        let new = "y\n".repeat(30_000);
        assert_eq!(
            churn(&old, &new),
            Churn {
                added: 30_000,
                removed: 30_000
            }
        );
        assert_eq!(diff(&old, &new), None);
    }

    #[test]
    fn carriage_returns_count_as_line_ends() {
        let old = "x\r".repeat(30_000);
        let new = "y\r".repeat(30_000);
        assert_eq!(diff(&old, &new), None);
        assert_eq!(line_breaks("a\r\nb\rc\nd"), 3);
    }
}
