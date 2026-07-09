//! Character-level refinement of paired removed and added lines.
//!
//! A removed line and the added line that replaces it usually differ in only a
//! few characters. Marking just those characters lets the renderer emphasize
//! the real change (a darker background over the differing spans) instead of
//! painting the whole line as changed. This walks the changed lines of a hunk,
//! pairs each removed line with the added line at the same position within the
//! run, and returns the differing byte ranges on each.

use std::ops::Range;

use similar::{DiffTag, TextDiff};

use crate::model::{DiffLine, LineKind};

/// The differing byte ranges within a single line's text.
pub type LineRanges = Vec<Range<usize>>;

/// The lowest word-level similarity at which a removed/added pair is refined.
/// Below this the two lines share too little for a within-line emphasis to be
/// meaningful, so the whole-line coloring speaks for itself.
const MIN_SIMILARITY: f32 = 0.5;

/// For each line in `lines`, the byte ranges of its text that differ from its
/// paired counterpart on the other side. The result is parallel to `lines`;
/// context lines, unpaired changed lines, and wholesale replacements get an
/// empty range list.
pub fn refine(lines: &[DiffLine]) -> Vec<LineRanges> {
    let mut out = vec![Vec::new(); lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Removed {
            i += 1;
            continue;
        }
        // A run of removed lines immediately followed by a run of added lines
        // is a replacement; pair them by position and refine each pair.
        let removed_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Removed {
            i += 1;
        }
        let added_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Added {
            i += 1;
        }
        let pairs = (added_start - removed_start).min(i - added_start);
        for offset in 0..pairs {
            let before = &lines[removed_start + offset];
            let after = &lines[added_start + offset];
            if let Some((before_ranges, after_ranges)) = refine_pair(&before.text, &after.text) {
                out[removed_start + offset] = before_ranges;
                out[added_start + offset] = after_ranges;
            }
        }
    }
    out
}

/// The differing byte ranges on each side of one removed/added pair, or `None`
/// when the two lines share too little to refine.
///
/// The comparison is word-level so a changed word emphasizes as a whole rather
/// than fragmenting on characters two words happen to share.
fn refine_pair(before: &str, after: &str) -> Option<(LineRanges, LineRanges)> {
    let diff = TextDiff::from_words(before, after);
    if diff.ratio() < MIN_SIMILARITY {
        return None;
    }
    let before_words: Vec<&str> = diff.old_slices().to_vec();
    let after_words: Vec<&str> = diff.new_slices().to_vec();
    let before_bytes = word_byte_offsets(&before_words);
    let after_bytes = word_byte_offsets(&after_words);
    let mut before_ranges = Vec::new();
    let mut after_ranges = Vec::new();
    for op in diff.ops() {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => {}
            DiffTag::Delete => {
                before_ranges.push(before_bytes[old_range.start]..before_bytes[old_range.end]);
            }
            DiffTag::Insert => {
                after_ranges.push(after_bytes[new_range.start]..after_bytes[new_range.end]);
            }
            DiffTag::Replace => {
                before_ranges.push(before_bytes[old_range.start]..before_bytes[old_range.end]);
                after_ranges.push(after_bytes[new_range.start]..after_bytes[new_range.end]);
            }
        }
    }
    Some((before_ranges, after_ranges))
}

/// The byte offset at which each word begins, plus the total byte length, so a
/// word-index range maps to a byte range.
fn word_byte_offsets(words: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(words.len() + 1);
    let mut byte = 0;
    for word in words {
        offsets.push(byte);
        byte += word.len();
    }
    offsets.push(byte);
    offsets
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::refine;
    use crate::line::LineNo;
    use crate::model::{DiffLine, LineKind};

    fn ln(n: u32) -> LineNo {
        LineNo::new(n).expect("nonzero line number")
    }

    /// A changed line: removed lines number the before side, added the after.
    fn line(kind: LineKind, text: &str, n: u32) -> DiffLine {
        let (old, new) = match kind {
            LineKind::Removed => (Some(ln(n)), None),
            LineKind::Added => (None, Some(ln(n))),
            LineKind::Context => (Some(ln(n)), Some(ln(n))),
        };
        DiffLine {
            kind,
            text: text.to_string(),
            old_lineno: old,
            new_lineno: new,
        }
    }

    #[test]
    fn refines_the_differing_word_of_a_replaced_line() {
        let lines = [
            line(LineKind::Removed, "hello there fred!", 1),
            line(LineKind::Added, "hello there pete!", 1),
        ];
        // The changed word (with its trailing "!") spans bytes 12..17.
        let expected: Vec<Vec<Range<usize>>> = vec![vec![12..17], vec![12..17]];
        k9::assert_equal!(refine(&lines), expected);
    }

    #[test]
    fn refines_an_insertion_and_deletion_within_a_line() {
        let lines = [
            line(LineKind::Removed, "let x = 1;", 1),
            line(LineKind::Added, "let x = 100;", 1),
        ];
        // The word "1;" becomes "100;", so the whole token is emphasized on
        // each side: bytes 8..10 before, 8..12 after.
        let expected: Vec<Vec<Range<usize>>> = vec![vec![8..10], vec![8..12]];
        k9::assert_equal!(refine(&lines), expected);
    }

    #[test]
    fn leaves_wholesale_replacements_unrefined() {
        let lines = [
            line(LineKind::Removed, "alpha", 1),
            line(LineKind::Added, "omega beta gamma", 1),
        ];
        let expected: Vec<Vec<Range<usize>>> = vec![vec![], vec![]];
        k9::assert_equal!(refine(&lines), expected);
    }

    #[test]
    fn leaves_context_and_unpaired_lines_unrefined() {
        let lines = [
            line(LineKind::Context, "unchanged", 1),
            line(LineKind::Removed, "only removed", 2),
            line(LineKind::Context, "also unchanged", 3),
        ];
        let expected: Vec<Vec<Range<usize>>> = vec![vec![], vec![], vec![]];
        k9::assert_equal!(refine(&lines), expected);
    }

    #[test]
    fn pairs_multiple_removed_and_added_lines_by_position() {
        let lines = [
            line(LineKind::Removed, "one fish", 1),
            line(LineKind::Removed, "two fish", 2),
            line(LineKind::Added, "one dish", 1),
            line(LineKind::Added, "two wish", 2),
        ];
        // Each pair differs in its second word ("fish" vs "dish"/"wish").
        let expected: Vec<Vec<Range<usize>>> = vec![vec![4..8], vec![4..8], vec![4..8], vec![4..8]];
        k9::assert_equal!(refine(&lines), expected);
    }
}
