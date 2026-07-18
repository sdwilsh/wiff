//! Moving a line-range comment forward from the diff version it was authored
//! against onto a newer one.
//!
//! The strategy is offset mapping first, then a fuzzy fallback, then a
//! cross-side bridge. We reconstruct the commented `(file, side)` content in
//! both versions, diff them with `similar`, and see where the anchored lines
//! end up: unchanged code maps exactly to its new position; changed code that
//! still resembles its captured snippet matches elsewhere (approximate). When
//! the reviewed code was rewritten past recognition on its own side, the bridge
//! traces the shared base between the versions to recover the position on the
//! new side (relocated); only when even that fails is the comment left pinned to
//! its last known location (outdated). A comment is never dropped, only
//! reclassified.

use std::collections::HashMap;

use similar::{DiffTag, TextDiff};
use wiff_diff::reconstitute::known_lines;
use wiff_diff::{Diff, FileDiff, LineNo, Side};

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
/// relocated when only the position survives via the shared base, and outdated
/// (pinned to the same location) otherwise.
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
    let old_file = old_diff.files.iter().find(|f| f.display_path() == file);
    let new_lines = known_lines(new_file, side);

    if let Some(old_file) = old_file
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

    if let Some(old_file) = old_file
        && let Some(outcome) = bridge_through_base(old_file, new_file, file, side, start)
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

/// Recover an after-side comment's position when its reviewed content was
/// rewritten past recognition, by tracing the base the two versions share. The
/// anchored line is pinned to the base line preceding it in the old version,
/// that base line is mapped onto the new version's base, and its position on
/// the new after side is where the comment is placed. The recovered target is a
/// single line, the place the reviewed code occupied; a multi-line range does
/// not keep its length, since the rewritten code has no faithful extent on the
/// new side. This finds nothing for a before-side comment, whose side is itself
/// the base, nor when the base line cannot be traced across the versions.
fn bridge_through_base(
    old_file: &FileDiff,
    new_file: &FileDiff,
    file: &str,
    side: Side,
    start: LineNo,
) -> Option<RebaseOutcome> {
    if side != Side::After {
        return None;
    }
    let base_old = base_line_before(old_file, start)?;
    let base_new = map_base_line(old_file, new_file, base_old)?;
    let landing = after_line_of_base(new_file, base_new)?;
    Some(RebaseOutcome {
        target: CommentTarget::Lines {
            file: file.to_string(),
            side: Side::After,
            start_line: landing,
            end_line: landing,
        },
        confidence: Confidence::Relocated,
    })
}

/// The before-side line number pinning where `after_line` sits in `file`: the
/// number of the last line preceding it that appears on the before side. An
/// added line has no before-side number of its own, so its nearest preceding
/// context or removed line locates it against the base. Accuracy falls off with
/// distance: when an unshown gap separates the anchored line from that nearest
/// before-side line, the recovered position is only as close as the gap is
/// small. Yields `None` when no before-side line precedes it (the range opens
/// the file) or when `after_line` is not shown in this diff.
fn base_line_before(file: &FileDiff, after_line: LineNo) -> Option<LineNo> {
    let mut base: Option<LineNo> = None;
    for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
        if line.new_lineno == Some(after_line) {
            return base;
        }
        if let Some(old) = line.old_lineno {
            base = Some(old);
        }
    }
    None
}

/// Map a before-side line number from `old_file`'s base onto `new_file`'s base,
/// following the runs the two bases share. Both versions diff against the same
/// base, so an unchanged base line keeps its identity even when its number
/// shifts; a base line rewritten between the versions has no counterpart and
/// yields `None`.
fn map_base_line(old_file: &FileDiff, new_file: &FileDiff, base: LineNo) -> Option<LineNo> {
    let old_before = known_lines(old_file, Side::Before);
    let new_before = known_lines(new_file, Side::Before);
    let base_index = old_before.iter().position(|(n, _)| *n == base)?;

    let old_texts: Vec<&str> = old_before.iter().map(|(_, t)| t.as_str()).collect();
    let new_texts: Vec<&str> = new_before.iter().map(|(_, t)| t.as_str()).collect();
    for op in TextDiff::from_slices(&old_texts, &new_texts).ops() {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if tag == DiffTag::Equal && old_range.contains(&base_index) {
            let new_index = new_range.start + (base_index - old_range.start);
            return Some(new_before[new_index].0);
        }
    }
    None
}

/// The after-side line number that a before-side `base` line maps to in `file`:
/// its own after number when it is a context line, otherwise the next following
/// line that appears on the after side. Yields `None` when the base line is not
/// shown in this diff or nothing after it reaches the after side.
fn after_line_of_base(file: &FileDiff, base: LineNo) -> Option<LineNo> {
    let mut seen = false;
    for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
        if line.old_lineno == Some(base) {
            seen = true;
        }
        if seen && let Some(new) = line.new_lineno {
            return Some(new);
        }
    }
    None
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

    fn parsed(text: &str) -> Diff {
        wiff_diff::parse::parse(text).expect("valid diff")
    }

    /// Build an after-side comment on lines `start..=end` of `f.rs` with
    /// `snippet` as its reviewed text and no captured context.
    fn after_comment(start: u32, end: u32, snippet: &[&str]) -> (CommentTarget, Anchor) {
        (
            after_target(start, end),
            Anchor {
                snippet: snippet.iter().map(|s| s.to_string()).collect(),
                context_before: vec![],
                context_after: vec![],
            },
        )
    }

    #[test]
    fn a_rewritten_after_line_is_relocated_through_the_shared_base() {
        // Both versions replace the body of `compute`, so v0's added lines are
        // gone from v1 and neither the offset map nor the snippet can find them.
        // The unchanged surrounding base (the `fn compute` line) moves the
        // comment onto the line that replaced the reviewed code in v1.
        let old_diff = parsed(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,4 @@\n fn compute() {\n-    todo!()\n+    let a = step_one();\n+    let b = step_two();\n }\n",
        );
        let new_diff = parsed(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n fn compute() {\n-    todo!()\n+    return calculate();\n }\n",
        );
        let (target, anchor) = after_comment(
            2,
            3,
            &["    let a = step_one();", "    let b = step_two();"],
        );
        wince::assert_eq!(
            rebase_line_comment(&target, Some(&anchor), &old_diff, &new_diff),
            Some(RebaseOutcome {
                target: after_target(2, 2),
                confidence: Confidence::Relocated,
            })
        );
    }

    #[test]
    fn a_multi_line_comment_collapses_onto_its_single_landing_line() {
        // The reviewed range spans two added lines that map to distinct spots
        // in v1: the first is rewritten in place, the second is deleted. The
        // bridge does not try to reconstruct a span for the vanished code; it
        // collapses the comment onto the one line the base points at.
        let old_diff = parsed(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,5 @@\n fn compute() {\n-    todo!()\n+    let a = step_one();\n+    let b = step_two();\n+    let c = step_three();\n }\n",
        );
        let new_diff = parsed(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n fn compute() {\n-    todo!()\n+    return calculate();\n }\n",
        );
        let (target, anchor) = after_comment(
            2,
            4,
            &[
                "    let a = step_one();",
                "    let b = step_two();",
                "    let c = step_three();",
            ],
        );
        wince::assert_eq!(
            rebase_line_comment(&target, Some(&anchor), &old_diff, &new_diff),
            Some(RebaseOutcome {
                target: after_target(2, 2),
                confidence: Confidence::Relocated,
            })
        );
    }

    #[test]
    fn a_context_base_lands_the_comment_on_the_context_line_itself() {
        // The added line's nearest preceding base is a context line, unchanged
        // on both sides. When the reviewed code is deleted in v1, the comment
        // moves onto that context line rather than after it.
        let old_diff = parsed(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,3 @@\n fn compute() {\n+    let a = step_one();\n }\n",
        );
        let new_diff = parsed(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n fn compute() {\n+    return calculate();\n }\n",
        );
        let (target, anchor) = after_comment(2, 2, &["    let a = step_one();"]);
        wince::assert_eq!(
            rebase_line_comment(&target, Some(&anchor), &old_diff, &new_diff),
            Some(RebaseOutcome {
                target: after_target(1, 1),
                confidence: Confidence::Relocated,
            })
        );
    }

    #[test]
    fn a_comment_whose_base_is_unshown_in_the_new_version_is_left_outdated() {
        // v1 touches an unrelated region, so the base line pinning the comment
        // is absent from v1's diff and cannot be traced across. Nothing locates
        // the reviewed code, so the comment stays outdated where it was.
        let old_diff = parsed(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,4 @@\n fn compute() {\n-    todo!()\n+    let a = step_one();\n+    let b = step_two();\n }\n",
        );
        let new_diff = parsed(
            "--- a/f.rs\n+++ b/f.rs\n@@ -4,3 +4,3 @@\n fn helper() {\n-    aux()\n+    aux2()\n }\n",
        );
        let (target, anchor) = after_comment(
            2,
            3,
            &["    let a = step_one();", "    let b = step_two();"],
        );
        wince::assert_eq!(
            rebase_line_comment(&target, Some(&anchor), &old_diff, &new_diff),
            Some(RebaseOutcome {
                target: after_target(2, 3),
                confidence: Confidence::Outdated,
            })
        );
    }
}
