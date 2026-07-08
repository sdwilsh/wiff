//! Reconstructing one side's content from a file's hunks.
//!
//! The diff is the source of truth, so a reconstruction is only as complete as
//! the captured context: lines the diff shows are [`ReconLine::Known`], and the
//! spans between hunks (and the open-ended tail) are [`ReconLine::Gap`]s. A
//! caller with access to the live file can fill the gaps; otherwise they render
//! as placeholders so a partial reconstruction never poses as the whole file.

use crate::line::LineNo;
use crate::model::{FileDiff, Side};

/// A reconstructed line, or a run of lines the diff omits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconLine {
    /// A line whose text the diff carries, with its number on the side.
    Known {
        /// The line's number on the reconstructed side.
        lineno: LineNo,
        /// The line's text, without a trailing newline.
        text: String,
    },
    /// A run of omitted lines. `count` is known for a gap between hunks, and
    /// `None` for the open-ended tail past the last hunk.
    Gap {
        /// The number of omitted lines, or `None` for the trailing gap.
        count: Option<u32>,
    },
}

/// Reconstruct `side` of `file` as an ordered sequence of known lines and gaps.
pub fn reconstitute(file: &FileDiff, side: Side) -> Vec<ReconLine> {
    let mut out = Vec::new();
    // The next line number we expect to emit on this side; hunks that start
    // later leave a gap of the intervening, unshown lines.
    let mut expected: u32 = 1;
    for hunk in &file.hunks {
        let start = match side {
            Side::Before => hunk.old_start,
            Side::After => hunk.new_start,
        };
        if start > expected {
            out.push(ReconLine::Gap {
                count: Some(start - expected),
            });
        }
        for line in &hunk.lines {
            if let Some(lineno) = line.lineno(side) {
                out.push(ReconLine::Known {
                    lineno,
                    text: line.text.clone(),
                });
                expected = lineno.get() + 1;
            }
        }
    }
    // Anything past the last hunk is unshown and of unknown length.
    out.push(ReconLine::Gap { count: None });
    out
}

/// The known lines of `side`, in order, dropping gaps. This is the content the
/// rebaser diffs old against new.
pub fn known_lines(file: &FileDiff, side: Side) -> Vec<(LineNo, String)> {
    reconstitute(file, side)
        .into_iter()
        .filter_map(|line| match line {
            ReconLine::Known { lineno, text } => Some((lineno, text)),
            ReconLine::Gap { .. } => None,
        })
        .collect()
}
