//! Rendering a diff into styled terminal lines.
//!
//! A [`DiffView`] turns a parsed [`Diff`] into a flat list of ratatui [`Line`]s
//! ready to scroll: a header per file, a header per hunk, and one row per diff
//! line with a line-number gutter, a change marker, and the file content
//! colored by syntect. Added and removed rows are tinted by role, and within a
//! replaced row the characters that actually changed are tinted more strongly,
//! from the word-level refinement in [`wiff_diff::intraline`].

use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use wiff_diff::{
    Diff, DiffLine, FileDiff, FileStatus, HighlightError, HighlightedLine, Highlighter, LineKind,
    LineNo, Rgb, Side, StyledSpan, intraline,
};

use crate::theme::Theme;

/// The gutter width for one side's line number.
const LINENO_WIDTH: usize = 4;

/// The width of the gutter before a content line: two line numbers, the change
/// marker, and the spaces separating them. A fold marker is indented this far so
/// it aligns under the code column.
const GUTTER_WIDTH: usize = LINENO_WIDTH * 2 + 4;

/// The shortest run of unchanged lines worth collapsing. Hiding a single line
/// behind a one-line marker saves nothing, so only runs of two or more fold.
const MIN_FOLD: usize = 2;

/// The context lines kept on each side of a change when nothing overrides it.
pub const DEFAULT_DISPLAY_CONTEXT: usize = 3;

/// A rendered diff: the styled lines to draw, paired one-to-one with the [`Row`]
/// metadata that says what each line is, plus the runs of unchanged lines that
/// can be folded away from view.
pub struct Document {
    /// The styled lines, in draw order.
    pub lines: Vec<Line<'static>>,
    /// The metadata for each line, parallel to `lines`.
    pub rows: Vec<Row>,
    /// The foldable runs of unchanged rows, in row order, non-overlapping.
    pub folds: Vec<Fold>,
}

/// A run of unchanged rows that can be collapsed behind a single marker line.
pub struct Fold {
    /// The first hidden row index into [`Document::rows`].
    pub start: usize,
    /// One past the last hidden row index.
    pub end: usize,
    /// The line shown in place of the hidden rows when collapsed.
    pub marker: Line<'static>,
}

/// What one rendered line corresponds to in the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The index of the file within the diff.
    pub file: usize,
    /// The kind of line and its place within the file.
    pub kind: RowKind,
}

/// The role of a rendered line: a file header, a hunk header, or a content line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    /// The header naming a file.
    FileHeader,
    /// A hunk's `@@` header.
    HunkHeader {
        /// The hunk's index within the file.
        hunk: usize,
    },
    /// A content line, anchorable to `(side, lineno)` on the diff.
    Content {
        /// The side the line's number belongs to.
        side: Side,
        /// The line's number on that side, absent for a malformed line.
        lineno: Option<LineNo>,
    },
}

impl Document {
    /// Append a styled line and its parallel row metadata.
    fn push(&mut self, file: usize, kind: RowKind, line: Line<'static>) {
        self.lines.push(line);
        self.rows.push(Row { file, kind });
    }
}

/// A renderer pairing a syntax highlighter with a color theme.
pub struct DiffView {
    highlighter: Highlighter,
    theme: Theme,
    display_context: usize,
}

impl DiffView {
    /// Build a renderer for `theme`, loading its syntect syntax theme, keeping
    /// the default number of context lines around each change.
    pub fn new(theme: Theme) -> Result<Self, HighlightError> {
        Ok(Self {
            highlighter: Highlighter::with_theme(&theme.syntax_theme)?,
            theme,
            display_context: DEFAULT_DISPLAY_CONTEXT,
        })
    }

    /// Keep `context` unchanged lines on each side of a change before folding the
    /// rest away.
    pub fn with_display_context(mut self, context: usize) -> Self {
        self.display_context = context;
        self
    }

    /// Render every file of `diff` into one scrollable [`Document`].
    pub fn render(&self, diff: &Diff) -> Document {
        let mut doc = Document {
            lines: Vec::new(),
            rows: Vec::new(),
            folds: Vec::new(),
        };
        for (index, file) in diff.files.iter().enumerate() {
            self.render_file(index, file, &mut doc);
        }
        doc
    }

    /// Append `file`'s header and hunks to `doc`.
    fn render_file(&self, index: usize, file: &FileDiff, doc: &mut Document) {
        doc.push(index, RowKind::FileHeader, self.file_header(file));
        let before = self.highlighter.highlight_side(file, Side::Before);
        let after = self.highlighter.highlight_side(file, Side::After);
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            doc.push(
                index,
                RowKind::HunkHeader { hunk: hunk_index },
                self.hunk_header(hunk),
            );
            let content_base = doc.rows.len();
            let emphasis = intraline::refine(&hunk.lines);
            for (line, ranges) in hunk.lines.iter().zip(&emphasis) {
                let (side, lineno, highlighted) = match line.kind {
                    LineKind::Removed => (
                        Side::Before,
                        line.old_lineno,
                        line.old_lineno.and_then(|n| before.get(&n)),
                    ),
                    LineKind::Context | LineKind::Added => (
                        Side::After,
                        line.new_lineno,
                        line.new_lineno.and_then(|n| after.get(&n)),
                    ),
                };
                doc.push(
                    index,
                    RowKind::Content { side, lineno },
                    self.content_line(line, highlighted, ranges),
                );
            }
            let kinds: Vec<LineKind> = hunk.lines.iter().map(|line| line.kind).collect();
            for run in foldable_runs(&kinds, self.display_context) {
                doc.folds.push(Fold {
                    start: content_base + run.start,
                    end: content_base + run.end,
                    marker: self.fold_marker(run.end - run.start),
                });
            }
        }
    }

    /// The line shown in place of `hidden` collapsed rows, indented to align
    /// under the code column.
    fn fold_marker(&self, hidden: usize) -> Line<'static> {
        let plural = if hidden == 1 { "" } else { "s" };
        let text = format!(
            "{:indent$}[{hidden} unchanged line{plural}]",
            "",
            indent = GUTTER_WIDTH
        );
        Line::from(Span::styled(
            text,
            Style::default().fg(color(self.theme.fold_fg)),
        ))
    }

    /// The header naming a file and how it changed.
    fn file_header(&self, file: &FileDiff) -> Line<'static> {
        let text = match file.status {
            FileStatus::Renamed => format!("renamed  {} -> {}", file.old_path, file.new_path),
            status => format!("{}  {}", status_label(status), file.display_path()),
        };
        Line::from(Span::styled(
            text,
            Style::default()
                .fg(color(self.theme.file_header_fg))
                .add_modifier(Modifier::BOLD),
        ))
    }

    /// The `@@` header locating a hunk, with its section heading when present.
    fn hunk_header(&self, hunk: &wiff_diff::Hunk) -> Line<'static> {
        let mut text = format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
        );
        if let Some(section) = &hunk.section {
            text.push(' ');
            text.push_str(section);
        }
        Line::from(Span::styled(
            text,
            Style::default().fg(color(self.theme.hunk_header_fg)),
        ))
    }

    /// One diff row: the gutter, the change marker, and the colored content,
    /// tinted by the line's role with changed characters emphasized.
    fn content_line(
        &self,
        line: &DiffLine,
        highlighted: Option<&HighlightedLine>,
        ranges: &[Range<usize>],
    ) -> Line<'static> {
        let (marker, row_bg, emphasis_bg) = match line.kind {
            LineKind::Context => (' ', None, None),
            LineKind::Added => (
                '+',
                Some(self.theme.added_bg),
                Some(self.theme.added_emphasis_bg),
            ),
            LineKind::Removed => (
                '-',
                Some(self.theme.removed_bg),
                Some(self.theme.removed_emphasis_bg),
            ),
        };
        let gutter_style = with_bg(Style::default().fg(color(self.theme.gutter_fg)), row_bg);
        let mut spans = vec![Span::styled(
            format!(
                "{} {} {} ",
                lineno(line.old_lineno),
                lineno(line.new_lineno),
                marker,
            ),
            gutter_style,
        )];
        for piece in split_pieces(
            highlighted.map(Vec::as_slice).unwrap_or(&[]),
            &line.text,
            ranges,
        ) {
            let bg = if piece.emphasized {
                emphasis_bg
            } else {
                row_bg
            };
            spans.push(Span::styled(
                piece.text,
                with_bg(Style::default().fg(color(piece.fg)), bg),
            ));
        }
        Line::from(spans)
    }
}

/// A run of content sharing one foreground color and emphasis state.
struct Piece {
    text: String,
    fg: Rgb,
    emphasized: bool,
}

/// Split a line's highlighted spans at the emphasis ranges, so each run is
/// wholly inside or outside a changed range. When highlighting produced no
/// spans (an empty content line), the raw text stands in with a neutral color.
fn split_pieces(spans: &[StyledSpan], text: &str, ranges: &[Range<usize>]) -> Vec<Piece> {
    if spans.is_empty() {
        return split_span(text, 0, NEUTRAL_FG, ranges);
    }
    let mut out = Vec::new();
    let mut offset = 0;
    for span in spans {
        out.extend(split_span(&span.text, offset, span.style.fg, ranges));
        offset += span.text.len();
    }
    out
}

/// Split one span, starting at byte `start` within the line, into pieces cut at
/// every emphasis-range boundary that falls inside it.
fn split_span(text: &str, start: usize, fg: Rgb, ranges: &[Range<usize>]) -> Vec<Piece> {
    let end = start + text.len();
    let mut cuts = vec![start, end];
    for range in ranges {
        if range.start > start && range.start < end {
            cuts.push(range.start);
        }
        if range.end > start && range.end < end {
            cuts.push(range.end);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .map(|pair| {
            let (from, to) = (pair[0], pair[1]);
            Piece {
                text: text[from - start..to - start].to_string(),
                fg,
                emphasized: ranges.iter().any(|r| r.start <= from && to <= r.end),
            }
        })
        .collect()
}

/// The neutral gray used for content when highlighting yields no spans.
const NEUTRAL_FG: Rgb = Rgb {
    r: 0xc0,
    g: 0xc5,
    b: 0xce,
};

/// A right-aligned line number, or blank space when the line is absent on this
/// side.
fn lineno(number: Option<wiff_diff::LineNo>) -> String {
    match number {
        Some(n) => format!("{:>width$}", n.get(), width = LINENO_WIDTH),
        None => " ".repeat(LINENO_WIDTH),
    }
}

/// The runs of unchanged content lines to fold away, given `kinds` for one
/// hunk's lines and the number of `context` lines to keep beside each change.
///
/// A line is kept when it is a change or within `context` lines of one; the
/// maximal runs of the remaining lines are folded, skipping any run too short to
/// be worth collapsing.
fn foldable_runs(kinds: &[LineKind], context: usize) -> Vec<Range<usize>> {
    let mut kept = vec![false; kinds.len()];
    for (i, kind) in kinds.iter().enumerate() {
        if matches!(kind, LineKind::Added | LineKind::Removed) {
            let lo = i.saturating_sub(context);
            let hi = (i + context + 1).min(kinds.len());
            for near in &mut kept[lo..hi] {
                *near = true;
            }
        }
    }
    let mut runs = Vec::new();
    let mut start = None;
    // A kept sentinel past the end closes any run still open at the last line.
    for (i, kept_here) in kept.iter().chain(std::iter::once(&true)).enumerate() {
        match (!kept_here, start) {
            (true, None) => start = Some(i),
            (false, Some(from)) => {
                if i - from >= MIN_FOLD {
                    runs.push(from..i);
                }
                start = None;
            }
            _ => {}
        }
    }
    runs
}

/// A short label for a file's change status.
fn status_label(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "added",
        FileStatus::Deleted => "deleted",
        FileStatus::Modified => "modified",
        FileStatus::Renamed => "renamed",
    }
}

/// Apply an optional background to a style, leaving it unset for context rows.
fn with_bg(style: Style, bg: Option<Rgb>) -> Style {
    match bg {
        Some(rgb) => style.bg(color(rgb)),
        None => style,
    }
}

/// Convert a wiff [`Rgb`] into a ratatui [`Color`].
pub(crate) fn color(rgb: Rgb) -> Color {
    Color::Rgb(rgb.r, rgb.g, rgb.b)
}

/// Rendering helpers shared by the render and app tests.
#[cfg(test)]
pub(crate) mod testutil {
    use ratatui::style::{Color, Modifier};
    use ratatui::text::Line;
    use wiff_diff::{DiffLine, FileDiff, FileStatus, Hunk, LineKind, LineNo};

    /// A line number from a nonzero `n`.
    pub(crate) fn ln(n: u32) -> LineNo {
        LineNo::new(n).expect("nonzero line number")
    }

    /// A one-hunk file over `lines`, each `(kind, text, lineno)`.
    pub(crate) fn file(
        path: &str,
        status: FileStatus,
        lines: &[(LineKind, &str, u32)],
    ) -> FileDiff {
        let diff_lines = lines
            .iter()
            .map(|(kind, text, n)| {
                let on_before = matches!(kind, LineKind::Context | LineKind::Removed);
                let on_after = matches!(kind, LineKind::Context | LineKind::Added);
                DiffLine {
                    kind: *kind,
                    text: (*text).to_string(),
                    old_lineno: on_before.then(|| ln(*n)),
                    new_lineno: on_after.then(|| ln(*n)),
                }
            })
            .collect();
        FileDiff {
            old_path: path.to_string(),
            new_path: path.to_string(),
            status,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: lines.len() as u32,
                new_start: 1,
                new_len: lines.len() as u32,
                section: None,
                lines: diff_lines,
            }],
        }
    }

    /// Serialize lines into one text row each, every span shown as
    /// `<fg|bg|mods>text` so the full visual result is asserted: the content,
    /// its colors, the role or selection tints, and bold.
    pub(crate) fn dump(lines: &[Line<'_>]) -> String {
        let mut out = String::new();
        for line in lines {
            for span in &line.spans {
                out.push_str(&format!(
                    "<{}|{}|{}>{}",
                    hex(span.style.fg),
                    hex(span.style.bg),
                    mods(span.style.add_modifier),
                    span.content,
                ));
            }
            out.push('\n');
        }
        out
    }

    /// A color as `#rrggbb`, or `-` when unset.
    fn hex(color: Option<Color>) -> String {
        match color {
            Some(Color::Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
            Some(other) => format!("{other:?}"),
            None => "-".to_string(),
        }
    }

    /// The set modifiers as short flags, or `-` when none.
    fn mods(modifier: Modifier) -> String {
        if modifier.contains(Modifier::BOLD) {
            "b".to_string()
        } else {
            "-".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use wiff_diff::{Diff, FileStatus, LineKind};

    use super::DiffView;
    use super::testutil::{dump, file};
    use crate::theme::Theme;

    #[test]
    fn renders_a_modified_file_with_headers_gutter_and_syntax_colors() {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Context, "let x = 1;", 1),
                    (LineKind::Added, "let y = 2;", 2),
                ],
            )],
        };
        let view = DiffView::new(Theme::dark()).unwrap();

        let expected = "\
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump(&view.render(&diff).lines), expected.to_string());
    }

    #[test]
    fn emphasizes_the_changed_characters_of_a_replaced_row() {
        let diff = Diff {
            files: vec![file(
                "greeting.txt",
                FileStatus::Modified,
                &[
                    (LineKind::Removed, "hello there fred", 1),
                    (LineKind::Added, "hello there pete", 1),
                ],
            )],
        };
        let view = DiffView::new(Theme::dark()).unwrap();

        // "fred"/"pete" (bytes 12..16) get the stronger emphasis background; the
        // unchanged run keeps the plain role tint. The .txt content has no
        // syntax, so it is one neutral color.
        let expected = "\
<#c0c5ce|-|b>modified  greeting.txt
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#65737e|#3b2d30|->   1      - <#c0c5ce|#3b2d30|->hello there <#c0c5ce|#5a3a40|->fred
<#65737e|#2d3b30|->        1 + <#c0c5ce|#2d3b30|->hello there <#c0c5ce|#3a5a40|->pete
";
        k9::assert_equal!(dump(&view.render(&diff).lines), expected.to_string());
    }

    #[test]
    fn foldable_runs_keep_context_around_changes_and_fold_the_rest() {
        use LineKind::{Added as A, Context as C};
        // Leading, interior, and trailing runs of context, keeping one line on
        // each side of the two changes.
        let kinds = [C, C, C, A, C, C, C, C, C, A, C, C, C];
        k9::assert_equal!(super::foldable_runs(&kinds, 1), vec![0..2, 5..8, 11..13]);
        // With enough context to reach across every gap, nothing folds.
        k9::assert_equal!(
            super::foldable_runs(&kinds, 5),
            Vec::<std::ops::Range<usize>>::new()
        );
    }
}
