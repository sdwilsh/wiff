//! Moving a line-range comment forward from the diff version it was authored
//! against onto a newer one.
//!
//! The strategy is offset mapping first, then a fuzzy fallback. We reconstruct
//! the commented `(file, side)` content in both versions, diff them with
//! `similar`, and see where the anchored lines land: unchanged code maps exactly
//! to its new position; changed or relocated code either matches its captured
//! snippet elsewhere (approximate) or is left pinned to its last known location
//! (outdated). A comment is never dropped, only reclassified.

use std::collections::HashMap;

use similar::{DiffTag, TextDiff};
use wiff_diff::reconstitute::known_lines;
use wiff_diff::{Diff, LineNo, Side};

use crate::record::{Anchor, CommentTarget, Confidence};

/// The lowest similarity at which the fuzzy fallback accepts a relocated
/// snippet as the same code rather than declaring the comment outdated.
const FUZZY_THRESHOLD: f32 = 0.6;

/// How much the surrounding context sways the fuzzy ranking relative to the
/// snippet's own similarity. The snippet dominates; context tips the choice
/// between windows of comparable snippet similarity, including toward an edited
/// copy over a stale but identical one elsewhere.
const CONTEXT_WEIGHT: f32 = 0.3;

/// Where a comment ends up after a refresh, and how confidently it got there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseOutcome {
    /// The comment's target in the new version.
    pub target: CommentTarget,
    /// How confidently it was relocated.
    pub confidence: Confidence,
}

/// Rebase `target` from `old_diff` onto `new_diff`. Only line-range comments
/// move; whole-file and review comments are not tied to line content, so this
/// returns `None` for them and they keep their existing target.
pub fn rebase_line_comment(
    target: &CommentTarget,
    anchor: Option<&Anchor>,
    old_diff: &Diff,
    new_diff: &Diff,
) -> Option<RebaseOutcome> {
    let CommentTarget::Lines {
        file,
        side,
        start_line,
        end_line,
    } = target
    else {
        return None;
    };
    Some(rebase_range(
        file,
        *side,
        *start_line,
        *end_line,
        anchor,
        old_diff,
        new_diff,
    ))
}

/// Rebase a single line range, always yielding an outcome: exact when the lines
/// map through unchanged code, approximate when the snippet is found relocated,
/// and outdated (pinned to the same location) otherwise.
fn rebase_range(
    file: &str,
    side: Side,
    start: LineNo,
    end: LineNo,
    anchor: Option<&Anchor>,
    old_diff: &Diff,
    new_diff: &Diff,
) -> RebaseOutcome {
    let pinned = CommentTarget::Lines {
        file: file.to_string(),
        side,
        start_line: start,
        end_line: end,
    };

    let Some(new_file) = new_diff.files.iter().find(|f| f.display_path() == file) else {
        return outdated(pinned);
    };
    let new_lines = known_lines(new_file, side);

    if let Some(old_file) = old_diff.files.iter().find(|f| f.display_path() == file)
        && let Some(outcome) = offset_map(
            &known_lines(old_file, side),
            &new_lines,
            file,
            side,
            start,
            end,
        )
    {
        return outcome;
    }

    if let Some(anchor) = anchor
        && let Some(outcome) = fuzzy_find(&new_lines, anchor, file, side)
    {
        return outcome;
    }

    outdated(pinned)
}

/// Map the old range onto the new side by diffing the two sides' content. This
/// yields an exact outcome only when every line of the range falls in an
/// unchanged run and maps to a contiguous new run; anything else returns `None`
/// so the caller can fall back.
fn offset_map(
    old_lines: &[(LineNo, String)],
    new_lines: &[(LineNo, String)],
    file: &str,
    side: Side,
    start: LineNo,
    end: LineNo,
) -> Option<RebaseOutcome> {
    let old_start = old_lines.iter().position(|(n, _)| *n == start)?;
    let old_end = old_lines.iter().position(|(n, _)| *n == end)?;

    let old_texts: Vec<&str> = old_lines.iter().map(|(_, t)| t.as_str()).collect();
    let new_texts: Vec<&str> = new_lines.iter().map(|(_, t)| t.as_str()).collect();
    let diff = TextDiff::from_slices(&old_texts, &new_texts);

    // HashMap<old_index, new_index>: for each line that is unchanged between the
    // two sides, which new index it maps to, keyed by its old index. Lines in
    // changed runs are absent, so a range touching changed code cannot be mapped
    // below.
    let mut equal_map: HashMap<usize, usize> = HashMap::new();
    for op in diff.ops() {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            for (offset, old_index) in old_range.enumerate() {
                equal_map.insert(old_index, new_range.start + offset);
            }
        }
    }

    // Every line of the range must map through the equal runs; a single missing
    // index means part of the range changed, so we give up and fall back.
    let new_indices: Vec<usize> = (old_start..=old_end)
        .map(|index| equal_map.get(&index).copied())
        .collect::<Option<_>>()?;
    // The mapped lines must stay contiguous in the new side. If they scattered,
    // the range was split apart between versions and no longer names one place.
    if !new_indices.windows(2).all(|pair| pair[1] == pair[0] + 1) {
        return None;
    }
    Some(RebaseOutcome {
        target: CommentTarget::Lines {
            file: file.to_string(),
            side,
            start_line: new_lines[new_indices[0]].0,
            end_line: new_lines[new_indices[new_indices.len() - 1]].0,
        },
        confidence: Confidence::Exact,
    })
}

/// Search the new side for the anchor's snippet, returning the best-matching
/// position as an approximate relocation. A window qualifies only when its
/// snippet similarity clears [`FUZZY_THRESHOLD`]; qualifiers are then ranked by a
/// blend of that similarity with how well the captured context flanks the
/// candidate, which distinguishes an edited or repeated snippet from a stale
/// lookalike elsewhere.
fn fuzzy_find(
    new_lines: &[(LineNo, String)],
    anchor: &Anchor,
    file: &str,
    side: Side,
) -> Option<RebaseOutcome> {
    let snippet: Vec<&str> = anchor.snippet.iter().map(String::as_str).collect();
    if snippet.is_empty() || new_lines.len() < snippet.len() {
        return None;
    }
    let before: Vec<&str> = anchor.context_before.iter().map(String::as_str).collect();
    let after: Vec<&str> = anchor.context_after.iter().map(String::as_str).collect();
    let new_texts: Vec<&str> = new_lines.iter().map(|(_, t)| t.as_str()).collect();

    // Slide a snippet-sized window across the new side. A window whose snippet
    // similarity clears the threshold is a candidate, scored by blending that
    // similarity with its surrounding context; the highest score wins, and a tie
    // keeps the earliest. The window slice cannot go out of bounds: the guard
    // above guarantees `snippet.len() <= new_texts.len()`, so the upper bound
    // never underflows.
    let mut best: Option<Match> = None;
    for offset in 0..=new_texts.len() - snippet.len() {
        let window = &new_texts[offset..offset + snippet.len()];
        // similar's ratio is 0.0 (nothing in common) to 1.0 (identical).
        let snippet_ratio = TextDiff::from_slices(&snippet, window).ratio();
        if snippet_ratio < FUZZY_THRESHOLD {
            continue;
        }
        // Absent context leaves the snippet score to stand alone rather than
        // penalizing a candidate for having no room beside it at a file edge.
        let score = match surrounding_ratio(&before, &after, &new_texts, offset, snippet.len()) {
            Some(context) => snippet_ratio * (1.0 - CONTEXT_WEIGHT) + context * CONTEXT_WEIGHT,
            None => snippet_ratio,
        };
        if best.is_none_or(|best| score > best.score) {
            best = Some(Match { score, offset });
        }
    }
    let best = best?;
    Some(RebaseOutcome {
        target: CommentTarget::Lines {
            file: file.to_string(),
            side,
            start_line: new_lines[best.offset].0,
            end_line: new_lines[best.offset + snippet.len() - 1].0,
        },
        confidence: Confidence::Approximate,
    })
}

/// Similarity of the captured context to the lines flanking a candidate snippet
/// at `offset`, weighted by how many context lines were actually available on
/// each side. Only the captured lines nearest the snippet are compared, so a
/// candidate against a file edge is scored on the context that fits rather than
/// against absent lines. Returns `None` when no context fits at all.
fn surrounding_ratio(
    before: &[&str],
    after: &[&str],
    new_texts: &[&str],
    offset: usize,
    snippet_len: usize,
) -> Option<f32> {
    // Keep the captured lines nearest the snippet: the last of `before` and the
    // first of `after`, as many as the new side leaves room for beside `offset`.
    let before_avail = before.len().min(offset);
    let before_window = &new_texts[offset - before_avail..offset];
    let before_near = &before[before.len() - before_avail..];

    let after_start = offset + snippet_len;
    let after_avail = after.len().min(new_texts.len() - after_start);
    let after_window = &new_texts[after_start..after_start + after_avail];
    let after_near = &after[..after_avail];

    let compared = before_avail + after_avail;
    if compared == 0 {
        return None;
    }
    let before_score = if before_avail == 0 {
        0.0
    } else {
        TextDiff::from_slices(before_near, before_window).ratio() * before_avail as f32
    };
    let after_score = if after_avail == 0 {
        0.0
    } else {
        TextDiff::from_slices(after_near, after_window).ratio() * after_avail as f32
    };
    Some((before_score + after_score) / compared as f32)
}

/// The best candidate found by the fuzzy fallback: its blended score and where
/// its snippet sits on the new side.
#[derive(Debug, Copy, Clone)]
struct Match {
    /// snippet similarity blended with surrounding context, 0.0 to 1.0.
    score: f32,
    /// index of the window's first line in the new side.
    offset: usize,
}

/// An outdated outcome: the reviewed code could not be located, so the comment
/// stays where it was, flagged for attention.
fn outdated(target: CommentTarget) -> RebaseOutcome {
    RebaseOutcome {
        target,
        confidence: Confidence::Outdated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Anchor;

    /// Number `texts` from line 1, as the fuzzy matcher sees a reconstructed side.
    fn numbered(texts: &[&str]) -> Vec<(LineNo, String)> {
        texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                (
                    LineNo::new(index as u32 + 1).expect("nonzero"),
                    text.to_string(),
                )
            })
            .collect()
    }

    fn after_target(start: u32, end: u32) -> CommentTarget {
        CommentTarget::Lines {
            file: "f.rs".to_string(),
            side: Side::After,
            start_line: LineNo::new(start).expect("nonzero"),
            end_line: LineNo::new(end).expect("nonzero"),
        }
    }

    #[test]
    fn leading_context_selects_the_matching_duplicate_snippet() {
        // A lone `}` appears twice; the captured context names the block the
        // comment sat in, so the second brace must win over the identical first.
        let new_lines = numbered(&[
            "fn b() {",
            "    step_two();",
            "}",
            "fn a() {",
            "    step_one();",
            "}",
        ]);
        let anchor = Anchor {
            snippet: vec!["}".to_string()],
            context_before: vec!["fn a() {".to_string(), "    step_one();".to_string()],
            context_after: vec![],
        };
        wince::assert_eq!(
            fuzzy_find(&new_lines, &anchor, "f.rs", Side::After),
            Some(RebaseOutcome {
                target: after_target(6, 6),
                confidence: Confidence::Approximate,
            })
        );
    }

    #[test]
    fn trailing_context_selects_the_matching_duplicate_snippet() {
        // `let x;` appears twice, distinguished only by the line that follows.
        let new_lines = numbered(&["let x;", "use_first();", "let x;", "use_second();"]);
        let anchor = Anchor {
            snippet: vec!["let x;".to_string()],
            context_before: vec![],
            context_after: vec!["use_second();".to_string()],
        };
        wince::assert_eq!(
            fuzzy_find(&new_lines, &anchor, "f.rs", Side::After),
            Some(RebaseOutcome {
                target: after_target(3, 3),
                confidence: Confidence::Approximate,
            })
        );
    }

    #[test]
    fn context_outweighs_a_stale_exact_duplicate_of_an_edited_snippet() {
        // The reviewed three-line snippet was edited in one line (similarity
        // below 1.0) and a byte-identical stale copy of the original survives
        // elsewhere. Snippet similarity alone would pick the stale copy; the
        // context flanking the edited copy must pull the match back to it.
        let new_lines = numbered(&[
            "fn other() {",
            "let sum = a",
            "    + b",
            "    + c;",
            "return other;",
            "fn total() {",
            "let sum = a",
            "    + b2",
            "    + c;",
            "return sum;",
        ]);
        let anchor = Anchor {
            snippet: vec![
                "let sum = a".to_string(),
                "    + b".to_string(),
                "    + c;".to_string(),
            ],
            context_before: vec!["fn total() {".to_string()],
            context_after: vec!["return sum;".to_string()],
        };
        wince::assert_eq!(
            fuzzy_find(&new_lines, &anchor, "f.rs", Side::After),
            Some(RebaseOutcome {
                target: after_target(7, 9),
                confidence: Confidence::Approximate,
            })
        );
    }

    #[test]
    fn without_context_the_first_snippet_match_is_taken() {
        // With no context to arbitrate, the equal matches tie and the earliest
        // wins, preserving the pre-context behavior.
        let new_lines = numbered(&["}", "keep", "}"]);
        let anchor = Anchor {
            snippet: vec!["}".to_string()],
            context_before: vec![],
            context_after: vec![],
        };
        wince::assert_eq!(
            fuzzy_find(&new_lines, &anchor, "f.rs", Side::After),
            Some(RebaseOutcome {
                target: after_target(1, 1),
                confidence: Confidence::Approximate,
            })
        );
    }

    #[test]
    fn a_snippet_absent_from_the_new_side_is_not_found() {
        let new_lines = numbered(&["alpha", "beta"]);
        let anchor = Anchor {
            snippet: vec!["gamma".to_string()],
            context_before: vec![],
            context_after: vec![],
        };
        wince::assert_eq!(fuzzy_find(&new_lines, &anchor, "f.rs", Side::After), None);
    }
}
