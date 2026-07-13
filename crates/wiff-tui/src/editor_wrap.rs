//! Soft word-wrapping for the inline comment editor.
//!
//! tui-textarea only horizontal-scrolls a logical line wider than the editor.
//! To wrap instead, each logical line is mapped into visual rows that fit a
//! column width, with a translation between the editor's char-indexed cursor
//! and the on-screen (row, column) position, so rendering and vertical
//! navigation can follow the wrapped shape rather than the logical lines.
//!
//! Wrapping breaks greedily at spaces, falling back to a hard break inside a
//! word longer than the width. Every character of a logical line belongs to
//! exactly one visual row (the space that triggers a soft break stays at the
//! end of the row it breaks after), so the char to visual mapping is a
//! bijection. Columns are counted in characters, matching the rest of the
//! renderer.

/// One on-screen row produced by wrapping the logical lines. The char range
/// indexes into `logical`'s line and abuts its neighbours on the same line.
pub(crate) struct VisualRow {
    /// Index of the logical line this row belongs to.
    pub logical: usize,
    /// Char offset into the logical line where this row starts.
    pub start_char: usize,
    /// Char offset one past this row's last char.
    pub end_char: usize,
    /// The text drawn for this row.
    pub text: String,
}

/// One visual row of a single logical line, before it is placed among the
/// other lines' rows. The char range indexes into that line.
struct LineRow {
    start_char: usize,
    end_char: usize,
    text: String,
}

/// Which visual row a cursor sitting exactly on a soft-wrap boundary belongs
/// to. An interior boundary's char index is shared by the end of one row and
/// the start of the next, so it maps to two visual positions; the bias picks
/// between them based on how the cursor arrived there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CursorBias {
    /// Resolve a boundary to the start of the following row. The default: after
    /// an edit, Home, or a horizontal move, a wrap point reads as the next row.
    Forward,
    /// Resolve a boundary to the end of the preceding row. Set after End or a
    /// vertical move leaves the cursor on a wrap point, so it stays on the row
    /// it moved onto rather than snapping to the next.
    Backward,
}

/// The wrapped view of the editor's logical lines at a given width, with the
/// mapping between the char-indexed logical cursor and visual coordinates.
pub(crate) struct WrapMap {
    rows: Vec<VisualRow>,
    /// For each logical line, the index of its first row in `rows`.
    line_first_row: Vec<usize>,
}

impl WrapMap {
    /// Wrap `lines` to `width` columns.
    pub(crate) fn build(lines: &[String], width: usize) -> Self {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut line_first_row = Vec::with_capacity(lines.len().max(1));
        for (logical, line) in lines.iter().enumerate() {
            line_first_row.push(rows.len());
            for row in wrap_line(line, width) {
                rows.push(VisualRow {
                    logical,
                    start_char: row.start_char,
                    end_char: row.end_char,
                    text: row.text,
                });
            }
        }
        // A textarea always holds at least one (possibly empty) line; guard the
        // empty-slice case so `rows` is never empty and indexing stays valid.
        if rows.is_empty() {
            line_first_row.push(0);
            rows.push(VisualRow {
                logical: 0,
                start_char: 0,
                end_char: 0,
                text: String::new(),
            });
        }
        WrapMap {
            rows,
            line_first_row,
        }
    }

    /// The total number of visual rows.
    pub(crate) fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The visual row at `index`.
    pub(crate) fn row(&self, index: usize) -> &VisualRow {
        &self.rows[index]
    }

    /// The visual (row, column) of the logical cursor at char position `ccol` on
    /// logical line `crow`. At the end of a logical line the cursor renders at
    /// the end of that line's last row; on an interior wrap boundary `bias`
    /// decides whether it renders at the preceding row's end or the following
    /// row's start.
    pub(crate) fn cursor_to_visual(
        &self,
        crow: usize,
        ccol: usize,
        bias: CursorBias,
    ) -> (usize, usize) {
        let first = self.line_first_row.get(crow).copied().unwrap_or(0);
        let last = self
            .line_first_row
            .get(crow + 1)
            .copied()
            .unwrap_or(self.rows.len());
        for vrow in first..last {
            let row = &self.rows[vrow];
            // A boundary position (ccol == end_char) belongs to this row only
            // under a backward bias; otherwise it falls through to the next
            // row's start. The last row of a line always claims it.
            let claims = match bias {
                CursorBias::Forward => ccol < row.end_char,
                CursorBias::Backward => ccol <= row.end_char,
            };
            if claims || vrow + 1 == last {
                return (vrow, ccol.saturating_sub(row.start_char));
            }
        }
        (first, 0)
    }

    /// The logical cursor for a vertical move that targets visual row `vrow` at
    /// column `goal`, together with the bias that keeps it rendering on `vrow`.
    /// A cursor at `vrow`'s trailing wrap boundary needs a backward bias so it
    /// stays there instead of snapping to the next row's start; every other
    /// position (including a leading boundary shared with the row above) reads
    /// forward.
    pub(crate) fn vertical_target(&self, vrow: usize, goal: usize) -> (usize, usize, CursorBias) {
        let vrow = vrow.min(self.rows.len().saturating_sub(1));
        let (logical, ccol) = self.visual_to_cursor(vrow, goal);
        let bias = if ccol == self.rows[vrow].end_char && !self.is_last_row_of_line(vrow) {
            CursorBias::Backward
        } else {
            CursorBias::Forward
        };
        (logical, ccol, bias)
    }

    /// The logical (line, char) at visual row `vrow` and column `vcol`, clamped
    /// to the row's end.
    pub(crate) fn visual_to_cursor(&self, vrow: usize, vcol: usize) -> (usize, usize) {
        let vrow = vrow.min(self.rows.len().saturating_sub(1));
        let row = &self.rows[vrow];
        let len = row.end_char - row.start_char;
        (row.logical, row.start_char + vcol.min(len))
    }

    /// The logical (line, char) for Home on visual row `vrow`: the start of the
    /// row.
    pub(crate) fn row_start_cursor(&self, vrow: usize) -> (usize, usize) {
        let row = &self.rows[vrow];
        (row.logical, row.start_char)
    }

    /// The logical (line, char) for End on visual row `vrow`. A soft-wrapped
    /// row's trailing break spaces are trimmed so the cursor stays on this row
    /// rather than jumping to the start of the next.
    pub(crate) fn row_end_cursor(&self, vrow: usize) -> (usize, usize) {
        let row = &self.rows[vrow];
        if self.is_last_row_of_line(vrow) {
            return (row.logical, row.end_char);
        }
        let trailing = row.text.chars().rev().take_while(|c| *c == ' ').count();
        (row.logical, row.end_char.saturating_sub(trailing))
    }

    /// Whether `vrow` is the last visual row of its logical line. An out-of-
    /// range row reads as last, since no row follows it.
    fn is_last_row_of_line(&self, vrow: usize) -> bool {
        let Some(row) = self.rows.get(vrow) else {
            return true;
        };
        match self.rows.get(vrow + 1) {
            Some(next) => next.logical != row.logical,
            None => true,
        }
    }
}

/// Wrap a single logical line into rows, breaking greedily at spaces and
/// hard-breaking inside an over-long word. An empty line yields one empty row.
fn wrap_line(line: &str, width: usize) -> Vec<LineRow> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![LineRow {
            start_char: 0,
            end_char: 0,
            text: String::new(),
        }];
    }

    let make_row = |start: usize, end: usize| -> LineRow {
        LineRow {
            start_char: start,
            end_char: end,
            text: chars[start..end].iter().collect(),
        }
    };

    let mut rows = Vec::new();
    let mut row_start = 0usize;
    // Char index to start the next row after the most recent space, or `None`
    // when no space break is available in the current row.
    let mut last_break: Option<usize> = None;
    let mut i = 0usize;
    while i < chars.len() {
        if i - row_start >= width && i > row_start {
            let break_at = match last_break {
                Some(b) if b > row_start => b,
                _ => i,
            };
            rows.push(make_row(row_start, break_at));
            row_start = break_at;
            // A space break starts the next row just past the most recent space
            // and a hard break moves nothing forward, so in either case the new
            // row opens with no earlier space to break on. The next space the
            // scan meets sets one; the width re-check on the next pass
            // hard-breaks a run past the break already wider than the row.
            last_break = None;
            continue;
        }
        if chars[i] == ' ' {
            last_break = Some(i + 1);
        }
        i += 1;
    }
    rows.push(make_row(row_start, chars.len()));
    rows
}

#[cfg(test)]
mod tests {
    use super::{CursorBias, WrapMap};

    /// The wrapped rows rendered as `logical: start..end text` lines for
    /// full-output assertions.
    fn rows(map: &WrapMap) -> String {
        (0..map.row_count())
            .map(|i| {
                let r = map.row(i);
                format!(
                    "{}: {}..{} {:?}",
                    r.logical, r.start_char, r.end_char, r.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_line_wraps_greedily_at_spaces() {
        let lines = vec!["hello world foo".to_string()];
        let map = WrapMap::build(&lines, 11);
        #[rustfmt::skip]
        wince::snapshot_str!(
            rows(&map),
            "0: 0..6 \"hello \"\n",
            "0: 6..15 \"world foo\"",
        );
    }

    #[test]
    fn an_over_long_word_hard_breaks_at_the_width() {
        let lines = vec!["abcdefghij".to_string()];
        let map = WrapMap::build(&lines, 4);
        #[rustfmt::skip]
        wince::snapshot_str!(
            rows(&map),
            "0: 0..4 \"abcd\"\n",
            "0: 4..8 \"efgh\"\n",
            "0: 8..10 \"ij\"",
        );
    }

    #[test]
    fn empty_and_multiple_logical_lines_each_yield_rows() {
        let lines = vec!["".to_string(), "hi".to_string()];
        let map = WrapMap::build(&lines, 10);
        #[rustfmt::skip]
        wince::snapshot_str!(
            rows(&map),
            "0: 0..0 \"\"\n",
            "1: 0..2 \"hi\"",
        );
    }

    #[test]
    fn the_cursor_maps_across_a_soft_wrap_boundary() {
        let lines = vec!["hello world foo".to_string()];
        let map = WrapMap::build(&lines, 11);

        // Start of "world" (char 6) is the start of the second visual row.
        k9::assert_equal!(map.cursor_to_visual(0, 6, CursorBias::Forward), (1, 0));
        // Inside the first row.
        k9::assert_equal!(map.cursor_to_visual(0, 3, CursorBias::Forward), (0, 3));
        // End of the line renders at the end of its last row.
        k9::assert_equal!(map.cursor_to_visual(0, 15, CursorBias::Forward), (1, 9));

        // Round-trips back to char positions.
        k9::assert_equal!(map.visual_to_cursor(1, 0), (0, 6));
        k9::assert_equal!(map.visual_to_cursor(0, 3), (0, 3));
    }

    #[test]
    fn a_boundary_position_resolves_by_cursor_bias() {
        // An over-long word hard-breaks so the wrap boundary (char 4) has no
        // trailing space: it is shared by row 0's end and row 1's start.
        let lines = vec!["abcdefghij".to_string()];
        let map = WrapMap::build(&lines, 4);

        // Arriving from the next row (moving up) keeps the cursor at the end of
        // row 0; arriving from an edit or horizontal move reads it as the start
        // of row 1.
        k9::assert_equal!(map.cursor_to_visual(0, 4, CursorBias::Backward), (0, 4));
        k9::assert_equal!(map.cursor_to_visual(0, 4, CursorBias::Forward), (1, 0));
    }

    #[test]
    fn a_vertical_move_biases_to_stay_on_the_target_row() {
        // A hard-broken word: rows 0..4, 4..8, 8..10 all share boundary chars.
        let lines = vec!["abcdefghij".to_string()];
        let map = WrapMap::build(&lines, 4);

        // Moving down to row 1 at column 0 reaches the boundary shared with
        // row 0's end; a forward bias keeps it at row 1's start.
        k9::assert_equal!(map.vertical_target(1, 0), (0, 4, CursorBias::Forward));
        // Moving up to row 1 at a column past its width reaches the boundary
        // shared with row 2's start; a backward bias keeps it at row 1's end.
        k9::assert_equal!(map.vertical_target(1, 99), (0, 8, CursorBias::Backward));
        // The last row of the line never needs a backward bias: its end is the
        // true line end.
        k9::assert_equal!(map.vertical_target(2, 99), (0, 10, CursorBias::Forward));
    }

    #[test]
    fn home_and_end_target_the_visual_row_bounds() {
        let lines = vec!["hello world foo".to_string()];
        let map = WrapMap::build(&lines, 11);

        // Home on the first row is its start; End trims the trailing break space
        // so the cursor stays on the first row (char 5, after "hello").
        k9::assert_equal!(map.row_start_cursor(0), (0, 0));
        k9::assert_equal!(map.row_end_cursor(0), (0, 5));

        // On the last row End is the true line end.
        k9::assert_equal!(map.row_start_cursor(1), (0, 6));
        k9::assert_equal!(map.row_end_cursor(1), (0, 15));
    }
}
