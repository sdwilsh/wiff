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

/// Search the new side for the anchor's captured snippet, accepting the most
/// similar window at or above [`FUZZY_THRESHOLD`] as an approximate relocation.
fn fuzzy_find(
    new_lines: &[(LineNo, String)],
    anchor: &Anchor,
    file: &str,
    side: Side,
) -> Option<RebaseOutcome> {
    let needle: Vec<&str> = anchor.snippet.iter().map(String::as_str).collect();
    if needle.is_empty() || new_lines.len() < needle.len() {
        return None;
    }
    let new_texts: Vec<&str> = new_lines.iter().map(|(_, t)| t.as_str()).collect();

    // Slide a needle-sized window across the new side and keep the most similar
    // one. The window slice cannot go out of bounds: the guard above guarantees
    // `needle.len() <= new_texts.len()`, so the upper bound never underflows.
    let mut best: Option<Match> = None;
    for offset in 0..=new_texts.len() - needle.len() {
        let window = &new_texts[offset..offset + needle.len()];
        // similar's ratio is 0.0 (nothing in common) to 1.0 (identical).
        let ratio = TextDiff::from_slices(&needle, window).ratio();
        if best.is_none_or(|best| ratio > best.ratio) {
            best = Some(Match { ratio, offset });
        }
    }
    let best = best?;
    if best.ratio < FUZZY_THRESHOLD {
        return None;
    }
    Some(RebaseOutcome {
        target: CommentTarget::Lines {
            file: file.to_string(),
            side,
            start_line: new_lines[best.offset].0,
            end_line: new_lines[best.offset + needle.len() - 1].0,
        },
        confidence: Confidence::Approximate,
    })
}

/// The best window found by the fuzzy fallback: where it starts on the new side
/// and how closely it matches the captured snippet.
#[derive(Debug, Clone, Copy)]
struct Match {
    /// similarity of the window to the snippet, 0.0 to 1.0.
    ratio: f32,
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
