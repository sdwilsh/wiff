//! Rendering a diff into styled terminal lines.
//!
//! A [`DiffView`] turns a parsed [`Diff`] into a flat list of ratatui [`Line`]s
//! ready to scroll: a header per file, a header per hunk, and one row per diff
//! line with a line-number gutter, a change marker, and the file content
//! colored by syntect. Added and removed rows are tinted by role, and within a
//! replaced row the characters that actually changed are tinted more strongly,
//! from the word-level refinement in [`wiff_diff::intraline`].
//!
//! Long runs of unchanged lines are marked foldable, and each fold marker names
//! the enclosing definition (function, struct, and so on) of the content below
//! it, recovered by [`wiff_diff::SectionMatchers`] since the wide-context
//! capture merges each file into one hunk with no per-change context header.

use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ulid::Ulid;
use wiff_core::record::{CommentTarget, Confidence};
use wiff_core::review::CommentState;
use wiff_diff::{
    Diff, DiffLine, FileDiff, FileStatus, HighlightError, HighlightedLine, Highlighter, LineKind,
    LineNo, Rgb, Section, SectionMatchers, Side, StyledSpan, intraline,
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

/// The file index for rows not tied to a file: the review summary and its
/// comments, which resolve to no path in the status line.
const NO_FILE: usize = usize::MAX;

/// The indent for comments not aligned to the code column, the review and
/// whole-file comments.
const COMMENT_INDENT: usize = 2;

/// The context lines kept on each side of a change when nothing overrides it.
pub const DEFAULT_DISPLAY_CONTEXT: usize = 3;

/// A rendered diff: the styled lines to draw, paired one-to-one with the [`Row`]
/// metadata that says what each line is, plus the runs of unchanged lines that
/// can be folded away from view.
pub struct Document {
    /// The styled lines, in draw order.
    pub lines: Vec<Line<'static>>,
    /// The background each line fills its whole row width with, parallel to
    /// `lines`; `None` for a row left to the terminal background.
    pub fills: Vec<Option<Rgb>>,
    /// The metadata for each line, parallel to `lines`.
    pub rows: Vec<Row>,
    /// The foldable runs of unchanged rows, in row order, non-overlapping.
    pub folds: Vec<Fold>,
    /// The collapsible comment bodies, in row order.
    pub comments: Vec<CommentRegion>,
    /// The display path of each file, indexed by [`Row::file`].
    pub files: Vec<String>,
}

/// A rendered comment: its header row and the body rows that collapse behind it.
pub struct CommentRegion {
    /// The annotation's stable identity.
    pub id: Ulid,
    /// The row index of the comment's header line.
    pub header: usize,
    /// The body rows hidden when the comment is collapsed: `[start, end)`.
    pub body: Range<usize>,
    /// Whether the comment starts collapsed (resolved comments do).
    pub collapsed_default: bool,
}

/// A run of unchanged rows that can be collapsed behind a single marker line.
pub struct Fold {
    /// The first hidden row index into [`Document::rows`].
    pub start: usize,
    /// One past the last hidden row index.
    pub end: usize,
    /// The line shown in place of the hidden rows when collapsed.
    pub marker: Line<'static>,
    /// The background the marker fills its whole row width with.
    pub fill: Option<Rgb>,
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
    /// The review summary row at the top of the document.
    ReviewSummary,
    /// A comment's header line, naming its author and status.
    CommentHeader {
        /// The annotation the header belongs to.
        id: Ulid,
    },
    /// One line of a comment's body.
    CommentBody {
        /// The annotation the body belongs to.
        id: Ulid,
    },
}

impl Document {
    /// Append a styled line, the background it fills its row with, and its
    /// parallel row metadata.
    fn push(&mut self, file: usize, kind: RowKind, fill: Option<Rgb>, line: Line<'static>) {
        self.lines.push(line);
        self.fills.push(fill);
        self.rows.push(Row { file, kind });
    }
}

/// A renderer pairing a syntax highlighter with a color theme.
pub struct DiffView {
    highlighter: Highlighter,
    theme: Theme,
    display_context: usize,
    sections: SectionMatchers,
}

impl DiffView {
    /// Build a renderer for `theme`, loading its syntect syntax theme, keeping
    /// the default number of context lines around each change.
    pub fn new(theme: Theme) -> Result<Self, HighlightError> {
        Ok(Self {
            highlighter: Highlighter::with_theme(&theme.syntax_theme)?,
            theme,
            display_context: DEFAULT_DISPLAY_CONTEXT,
            sections: SectionMatchers::builtins(),
        })
    }

    /// Keep `context` unchanged lines on each side of a change before folding the
    /// rest away.
    pub fn with_display_context(mut self, context: usize) -> Self {
        self.display_context = context;
        self
    }

    /// Recognise enclosing-definition lines with `sections`, so fold markers name
    /// the scope the hidden lines sit in.
    pub fn with_section_matchers(mut self, sections: SectionMatchers) -> Self {
        self.sections = sections;
        self
    }

    /// Render every file of `diff` into one scrollable [`Document`], with no
    /// review overlay.
    pub fn render(&self, diff: &Diff) -> Document {
        self.build(diff, &[], &[], false)
    }

    /// Render `diff` with `comments` woven in: a review summary row at the top,
    /// whole-file comments under their file header, and line comments in a block
    /// above the line they anchor. Deleted comments are the caller's to filter.
    /// Comments whose id is in `pending` are badged as uncommitted drafts.
    pub fn render_review(
        &self,
        diff: &Diff,
        comments: &[CommentState],
        pending: &[Ulid],
    ) -> Document {
        self.build(diff, comments, pending, true)
    }

    /// The shared render path: build the document, optionally leading with the
    /// review summary row, then each file with its placed comments.
    fn build(
        &self,
        diff: &Diff,
        comments: &[CommentState],
        pending: &[Ulid],
        review_row: bool,
    ) -> Document {
        let mut doc = Document {
            lines: Vec::new(),
            fills: Vec::new(),
            rows: Vec::new(),
            folds: Vec::new(),
            comments: Vec::new(),
            files: diff
                .files
                .iter()
                .map(|f| f.display_path().to_string())
                .collect(),
        };
        let placement = Placement::new(diff, comments);
        if review_row {
            doc.push(
                NO_FILE,
                RowKind::ReviewSummary,
                Some(self.theme.status_bg),
                self.review_summary(),
            );
            for comment in &placement.review {
                self.push_comment(
                    &mut doc,
                    NO_FILE,
                    comment,
                    COMMENT_INDENT,
                    pending.contains(&comment.id),
                );
            }
        }
        for (index, file) in diff.files.iter().enumerate() {
            self.render_file(index, file, &placement.files[index], pending, &mut doc);
        }
        doc
    }

    /// Append `file`'s header, its whole-file and floated comments, and its
    /// hunks with any line comments woven in.
    fn render_file(
        &self,
        index: usize,
        file: &FileDiff,
        placement: &FilePlacement,
        pending: &[Ulid],
        doc: &mut Document,
    ) {
        doc.push(index, RowKind::FileHeader, None, self.file_header(file));
        for comment in &placement.header {
            self.push_comment(
                doc,
                index,
                comment,
                COMMENT_INDENT,
                pending.contains(&comment.id),
            );
        }
        let before = self.highlighter.highlight_side(file, Side::Before);
        let after = self.highlighter.highlight_side(file, Side::After);
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            doc.push(
                index,
                RowKind::HunkHeader { hunk: hunk_index },
                None,
                self.hunk_header(hunk),
            );
            let emphasis = intraline::refine(&hunk.lines);
            let mut line_row = Vec::with_capacity(hunk.lines.len());
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
                if let Some(n) = lineno {
                    for comment in placement.at(side, n.get()) {
                        self.push_comment(
                            doc,
                            index,
                            comment,
                            GUTTER_WIDTH,
                            pending.contains(&comment.id),
                        );
                    }
                }
                line_row.push(doc.rows.len());
                let (rendered, fill) = self.content_line(line, highlighted, ranges);
                doc.push(index, RowKind::Content { side, lineno }, fill, rendered);
            }
            let kinds: Vec<LineKind> = hunk.lines.iter().map(|line| line.kind).collect();
            let anchored: Vec<bool> = hunk
                .lines
                .iter()
                .map(|line| line_anchor(line).is_some_and(|(s, n)| placement.covers(s, n)))
                .collect();
            let section = self.sections.for_path(file.display_path());
            for run in foldable_runs(&kinds, self.display_context, &anchored) {
                let scope = enclosing_scope(&hunk.lines, run.end, &section);
                doc.folds.push(Fold {
                    start: line_row[run.start],
                    end: line_row[run.end - 1] + 1,
                    marker: self.fold_marker(run.end - run.start, scope),
                    fill: Some(self.theme.status_bg),
                });
            }
        }
    }

    /// The review summary row heading, the top-of-document target for review
    /// comments and the jump-to-top landing spot, backed by the status bar color
    /// with a dimmed hint at how to draft a review-level comment.
    fn review_summary(&self) -> Line<'static> {
        let bg = color(self.theme.status_bg);
        Line::from(vec![
            Span::styled(
                "Review",
                Style::default()
                    .fg(color(self.theme.review_fg))
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " [press c here to draft the review comment]",
                Style::default().fg(color(self.theme.fold_fg)).bg(bg),
            ),
        ])
    }

    /// Append `comment` as a header row plus its body rows, indented by `indent`,
    /// and record the collapsible region so a resolved comment starts collapsed.
    fn push_comment(
        &self,
        doc: &mut Document,
        file: usize,
        comment: &CommentState,
        indent: usize,
        pending: bool,
    ) {
        let header = doc.rows.len();
        doc.push(
            file,
            RowKind::CommentHeader { id: comment.id },
            None,
            self.comment_header(comment, indent, pending),
        );
        let body_start = doc.rows.len();
        for text in comment.body.trim_end().split('\n') {
            doc.push(
                file,
                RowKind::CommentBody { id: comment.id },
                None,
                self.comment_body(text, indent),
            );
        }
        doc.comments.push(CommentRegion {
            id: comment.id,
            header,
            body: body_start..doc.rows.len(),
            collapsed_default: comment.resolved || comment.deleted,
        });
    }

    /// A comment's header line: a marker, the author and kind, and status badges.
    fn comment_header(
        &self,
        comment: &CommentState,
        indent: usize,
        pending: bool,
    ) -> Line<'static> {
        let mut spans = vec![
            Span::styled(
                format!("{:indent$}* ", ""),
                Style::default().fg(color(self.theme.comment_flag_fg)),
            ),
            Span::styled(
                format!("{} ({})", comment.author.name, comment.author.kind.as_str()),
                Style::default().fg(color(self.theme.comment_author_fg)),
            ),
        ];
        for (text, style) in badges(comment, pending) {
            let fg = match style {
                BadgeStyle::Muted => self.theme.comment_flag_fg,
                BadgeStyle::Warn => self.theme.comment_warn_fg,
                BadgeStyle::Draft => self.theme.comment_draft_fg,
            };
            spans.push(Span::styled(
                format!(" [{text}]"),
                Style::default().fg(color(fg)),
            ));
        }
        Line::from(spans)
    }

    /// One line of a comment's body, indented under its header.
    fn comment_body(&self, text: &str, indent: usize) -> Line<'static> {
        Line::from(Span::styled(
            format!("{:indent$}  {text}", ""),
            Style::default().fg(color(self.theme.comment_fg)),
        ))
    }

    /// The line shown in place of `hidden` collapsed rows, indented to align
    /// under the code column and naming the enclosing `scope` when one is known.
    fn fold_marker(&self, hidden: usize, scope: Option<&str>) -> Line<'static> {
        let plural = if hidden == 1 { "" } else { "s" };
        let mut text = format!(
            "{:indent$}[{hidden} unchanged line{plural}]",
            "",
            indent = GUTTER_WIDTH
        );
        if let Some(scope) = scope {
            text.push_str("  ");
            text.push_str(scope);
        }
        Line::from(Span::styled(
            text,
            Style::default()
                .fg(color(self.theme.fold_fg))
                .bg(color(self.theme.status_bg)),
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
    /// tinted by the line's role with changed characters emphasized. Also
    /// returns the background the whole row fills its width with, so the tint
    /// reaches the edge of the screen behind a shorter line.
    fn content_line(
        &self,
        line: &DiffLine,
        highlighted: Option<&HighlightedLine>,
        ranges: &[Range<usize>],
    ) -> (Line<'static>, Option<Rgb>) {
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
        let gutter = Span::styled(
            format!(
                "{} {} {} ",
                lineno(line.old_lineno),
                lineno(line.new_lineno),
                marker,
            ),
            gutter_style,
        );
        let mut content = Vec::new();
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
            content.push(Span::styled(
                piece.text,
                with_bg(Style::default().fg(color(piece.fg)), bg),
            ));
        }
        // Flag trailing whitespace a change introduces, the way `git diff` warns
        // on it, since it is easy to add and hard to see.
        if line.kind == LineKind::Added {
            mark_trailing_whitespace(&mut content, &line.text, self.theme.whitespace_bg);
        }
        let mut spans = vec![gutter];
        spans.extend(content);
        (Line::from(spans), row_bg)
    }
}

/// Recolor the background of any trailing whitespace in `spans`, whose text
/// concatenates to the row content, so it stands out. Splits the span the
/// whitespace begins in when it starts mid-span.
fn mark_trailing_whitespace(spans: &mut Vec<Span<'static>>, text: &str, bg: Rgb) {
    let trail = text.trim_end_matches([' ', '\t']).len();
    if trail == text.len() {
        return;
    }
    let mut out = Vec::with_capacity(spans.len());
    let mut offset = 0;
    for span in spans.drain(..) {
        let start = offset;
        let end = offset + span.content.len();
        offset = end;
        if end <= trail {
            out.push(span);
        } else if start >= trail {
            out.push(Span::styled(span.content, span.style.bg(color(bg))));
        } else {
            let cut = trail - start;
            let content = span.content.into_owned();
            out.push(Span::styled(content[..cut].to_string(), span.style));
            out.push(Span::styled(
                content[cut..].to_string(),
                span.style.bg(color(bg)),
            ));
        }
    }
    *spans = out;
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
/// hunk's lines, the number of `context` lines to keep beside each change, and
/// which lines are `anchored` by a comment.
///
/// A line is kept when it is a change, when it carries a comment, or when it is
/// within `context` lines of either; the maximal runs of the remaining lines are
/// folded, skipping any run too short to be worth collapsing. Keeping a comment's
/// line splits an otherwise-foldable run around it rather than hiding it.
fn foldable_runs(kinds: &[LineKind], context: usize, anchored: &[bool]) -> Vec<Range<usize>> {
    let mut kept = vec![false; kinds.len()];
    for (i, kind) in kinds.iter().enumerate() {
        if matches!(kind, LineKind::Added | LineKind::Removed) || anchored[i] {
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

/// The enclosing definition for the content just below a fold: the nearest
/// after-side line above `below` that `section` recognises, trimmed. Removed
/// lines are skipped since they are gone from the content the reviewer reads.
fn enclosing_scope<'a>(lines: &'a [DiffLine], below: usize, section: &Section) -> Option<&'a str> {
    (0..below)
        .rev()
        .filter(|&i| !matches!(lines[i].kind, LineKind::Removed))
        .map(|i| lines[i].text.as_str())
        .find(|text| section.is_definition(text))
        .map(str::trim)
}

/// The `(side, lineno)` a diff line is addressed by, matching how content rows
/// are emitted: removed lines by their before number, context and added lines by
/// their after number.
fn line_anchor(line: &DiffLine) -> Option<(Side, u32)> {
    match line.kind {
        LineKind::Removed => line.old_lineno.map(|n| (Side::Before, n.get())),
        LineKind::Context | LineKind::Added => line.new_lineno.map(|n| (Side::After, n.get())),
    }
}

/// The `(side, lineno)` pairs a file renders, so a line comment whose anchor is
/// gone can be floated to the file header rather than dropped.
fn rendered_anchors(file: &FileDiff) -> Vec<(Side, u32)> {
    file.hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .filter_map(line_anchor)
        .collect()
}

/// How a comment badge is tinted.
enum BadgeStyle {
    /// A muted badge such as `resolved`.
    Muted,
    /// A warning badge such as `shifted` or `outdated`.
    Warn,
    /// The `draft` badge for a comment with uncommitted edits.
    Draft,
}

/// A comment's status badges, in display order. A pending comment leads with a
/// `draft` badge so uncommitted work stands out. A deleted comment shows only
/// that it is withdrawn, its other status being moot until it is restored.
fn badges(comment: &CommentState, pending: bool) -> Vec<(&'static str, BadgeStyle)> {
    let mut out = Vec::new();
    if pending {
        out.push(("draft", BadgeStyle::Draft));
    }
    if comment.deleted {
        out.push(("deleted", BadgeStyle::Muted));
        return out;
    }
    if comment.resolved {
        out.push(("resolved", BadgeStyle::Muted));
    }
    match comment.confidence {
        Some(Confidence::Approximate) => out.push(("shifted", BadgeStyle::Warn)),
        Some(Confidence::Outdated) => out.push(("outdated", BadgeStyle::Warn)),
        Some(Confidence::Exact) | None => {}
    }
    out
}

/// A live line comment with the range it anchors, so a fold splits around it.
struct LineComment<'a> {
    side: Side,
    start: u32,
    end: u32,
    comment: &'a CommentState,
}

/// Where each live comment attaches within one render.
struct Placement<'a> {
    /// Review-level comments, shown under the summary row.
    review: Vec<&'a CommentState>,
    /// Per-file placement, indexed by file index.
    files: Vec<FilePlacement<'a>>,
}

/// One file's placed comments.
#[derive(Default)]
struct FilePlacement<'a> {
    /// Whole-file comments, and line comments whose anchor no longer matches a
    /// rendered line, shown under the file header.
    header: Vec<&'a CommentState>,
    /// Line comments, each above the line it anchors.
    lines: Vec<LineComment<'a>>,
}

impl<'a> Placement<'a> {
    /// Sort `comments` into review, whole-file, and per-line placement. A line
    /// comment whose anchored line is no longer rendered floats to its file
    /// header. A comment naming an unknown file is skipped.
    fn new(diff: &Diff, comments: &'a [CommentState]) -> Self {
        let mut files: Vec<FilePlacement<'a>> = (0..diff.files.len())
            .map(|_| FilePlacement::default())
            .collect();
        let addressable: Vec<Vec<(Side, u32)>> = diff.files.iter().map(rendered_anchors).collect();
        let index_of = |path: &str| diff.files.iter().position(|f| f.display_path() == path);
        let mut review = Vec::new();
        for comment in comments {
            match &comment.target {
                CommentTarget::Review => review.push(comment),
                CommentTarget::File { file } => {
                    if let Some(i) = index_of(file) {
                        files[i].header.push(comment);
                    }
                }
                CommentTarget::Lines {
                    file,
                    side,
                    start_line,
                    end_line,
                } => {
                    let Some(i) = index_of(file) else { continue };
                    let (start, end) = (start_line.get(), end_line.get());
                    if addressable[i].contains(&(*side, start)) {
                        files[i].lines.push(LineComment {
                            side: *side,
                            start,
                            end,
                            comment,
                        });
                    } else {
                        files[i].header.push(comment);
                    }
                }
            }
        }
        Self { review, files }
    }
}

impl<'a> FilePlacement<'a> {
    /// The comments anchored to start at the line `(side, lineno)` addresses.
    fn at(&self, side: Side, lineno: u32) -> impl Iterator<Item = &'a CommentState> + '_ {
        self.lines
            .iter()
            .filter(move |lc| lc.side == side && lc.start == lineno)
            .map(|lc| lc.comment)
    }

    /// Whether any line comment's range covers `(side, lineno)`, keeping the line
    /// out of a fold.
    fn covers(&self, side: Side, lineno: u32) -> bool {
        self.lines
            .iter()
            .any(|lc| lc.side == side && lc.start <= lineno && lineno <= lc.end)
    }
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
    use ratatui::text::Line;
    use ulid::Ulid;
    use wiff_core::record::{Author, AuthorKind, CommentTarget, Confidence};
    use wiff_core::review::CommentState;
    use wiff_diff::{Diff, FileStatus, LineKind, Side};

    use super::testutil::{dump, file, ln};
    use super::{CommentRegion, DiffView};
    use crate::theme::Theme;

    /// A comment with the given identity, author, target, and body; not resolved
    /// and exactly anchored unless the test overrides those fields.
    fn comment(
        id: u128,
        author: (&str, AuthorKind),
        target: CommentTarget,
        body: &str,
    ) -> CommentState {
        CommentState {
            id: Ulid(id),
            author: Author {
                name: author.0.to_string(),
                kind: author.1,
            },
            target,
            version: 0,
            anchor: None,
            body: body.to_string(),
            resolved: false,
            deleted: false,
            confidence: None,
            created_seq: 0,
            updated_seq: 0,
        }
    }

    /// A line-range target on the after side over `start..=end` of `file`.
    fn on_lines(file: &str, start: u32, end: u32) -> CommentTarget {
        CommentTarget::Lines {
            file: file.to_string(),
            side: Side::After,
            start_line: ln(start),
            end_line: ln(end),
        }
    }

    /// Each comment region as `id: header,body_start..body_end collapsed=<bool>`,
    /// so the collapsible structure is asserted alongside the rendered lines.
    fn regions(regions: &[CommentRegion]) -> String {
        let mut out = String::new();
        for region in regions {
            out.push_str(&format!(
                "{}: header {} body {}..{} collapsed={}\n",
                region.id.0,
                region.header,
                region.body.start,
                region.body.end,
                region.collapsed_default,
            ));
        }
        out
    }

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
    fn trailing_whitespace_on_an_added_line_is_flagged() {
        // The added line ends in two spaces, which get the whitespace warning
        // background; the removed line's trailing space is left alone since the
        // warning is only about whitespace a change introduces.
        let diff = Diff {
            files: vec![file(
                "notes.txt",
                FileStatus::Modified,
                &[
                    (LineKind::Removed, "old ", 1),
                    (LineKind::Added, "new  ", 1),
                ],
            )],
        };
        let view = DiffView::new(Theme::dark()).unwrap();
        let expected = "\
<#c0c5ce|-|b>modified  notes.txt
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#65737e|#3b2d30|->   1      - <#c0c5ce|#3b2d30|->old 
<#65737e|#2d3b30|->        1 + <#c0c5ce|#2d3b30|->new<#c0c5ce|#9a2a2a|->  
";
        k9::assert_equal!(dump(&view.render(&diff).lines), expected.to_string());
    }

    #[test]
    fn a_fold_marker_names_the_enclosing_definition_of_the_content_below_it() {
        // A rust change buried below its function: the leading context folds and
        // its marker names the enclosing fn, found by scanning up past the
        // hidden lines.
        let mut lines: Vec<(LineKind, String, u32)> =
            vec![(LineKind::Context, "fn draw() {".to_string(), 1)];
        for n in 2..=6 {
            lines.push((LineKind::Context, format!("    let v{n} = {n};"), n));
        }
        lines.push((LineKind::Added, "    let w = 7;".to_string(), 7));
        let borrowed: Vec<(LineKind, &str, u32)> =
            lines.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect();
        let diff = Diff {
            files: vec![file("src/lib.rs", FileStatus::Modified, &borrowed)],
        };
        let doc = DiffView::new(Theme::dark()).unwrap().render(&diff);
        let markers: Vec<Line<'static>> = doc.folds.iter().map(|f| f.marker.clone()).collect();
        let expected = "\
<#8a8a8a|#343d46|->            [3 unchanged lines]  fn draw() {
";
        k9::assert_equal!(dump(&markers), expected.to_string());
    }

    #[test]
    fn foldable_runs_keep_context_around_changes_and_fold_the_rest() {
        use LineKind::{Added as A, Context as C};
        // Leading, interior, and trailing runs of context, keeping one line on
        // each side of the two changes.
        let kinds = [C, C, C, A, C, C, C, C, C, A, C, C, C];
        let none = vec![false; kinds.len()];
        k9::assert_equal!(
            super::foldable_runs(&kinds, 1, &none),
            vec![0..2, 5..8, 11..13]
        );
        // With enough context to reach across every gap, nothing folds.
        k9::assert_equal!(
            super::foldable_runs(&kinds, 5, &none),
            Vec::<std::ops::Range<usize>>::new()
        );
    }

    #[test]
    fn a_line_comment_renders_in_a_block_above_the_line_it_anchors() {
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
        let comments = vec![comment(
            1,
            ("wez", AuthorKind::Human),
            on_lines("src/lib.rs", 2, 2),
            "why 2?",
        )];
        let doc = DiffView::new(Theme::dark())
            .unwrap()
            .render_review(&diff, &comments, &[]);
        let lines = "\
<#ebcb8b|#343d46|b>Review<#8a8a8a|#343d46|-> [press c here to draft the review comment]
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|-|->            * <#8fa1b3|-|->wez (human)
<#c0c5ce|-|->              why 2?
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump(&doc.lines), lines.to_string());
        // The header row precedes the single body row, which is the collapsible
        // range; an unresolved comment starts expanded.
        let expected_regions = "1: header 4 body 5..6 collapsed=false\n";
        k9::assert_equal!(regions(&doc.comments), expected_regions.to_string());
    }

    #[test]
    fn a_pending_comment_leads_its_badges_with_a_draft_flag() {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Added, "let y = 2;", 1)],
            )],
        };
        let comments = vec![comment(
            1,
            ("wez", AuthorKind::Human),
            on_lines("src/lib.rs", 1, 1),
            "why 2?",
        )];
        let doc = DiffView::new(Theme::dark())
            .unwrap()
            .render_review(&diff, &comments, &[Ulid(1)]);
        let lines = "\
<#ebcb8b|#343d46|b>Review<#8a8a8a|#343d46|-> [press c here to draft the review comment]
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,1 +1,1 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->wez (human)<#a3be8c|-|-> [draft]
<#c0c5ce|-|->              why 2?
<#65737e|#2d3b30|->        1 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump(&doc.lines), lines.to_string());
    }

    #[test]
    fn a_resolved_comment_region_starts_collapsed() {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Added, "let y = 2;", 1)],
            )],
        };
        let mut resolved = comment(
            7,
            ("opus", AuthorKind::Agent),
            on_lines("src/lib.rs", 1, 1),
            "done",
        );
        resolved.resolved = true;
        let doc = DiffView::new(Theme::dark())
            .unwrap()
            .render_review(&diff, &[resolved], &[]);
        let lines = "\
<#ebcb8b|#343d46|b>Review<#8a8a8a|#343d46|-> [press c here to draft the review comment]
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,1 +1,1 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->opus (agent)<#8a8a8a|-|-> [resolved]
<#c0c5ce|-|->              done
<#65737e|#2d3b30|->        1 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump(&doc.lines), lines.to_string());
        let expected_regions = "7: header 3 body 4..5 collapsed=true\n";
        k9::assert_equal!(regions(&doc.comments), expected_regions.to_string());
    }

    #[test]
    fn the_review_summary_row_leads_the_document_and_carries_review_comments() {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Added, "let y = 2;", 1)],
            )],
        };
        let comments = vec![comment(
            3,
            ("wez", AuthorKind::Human),
            CommentTarget::Review,
            "looks good overall",
        )];
        let doc = DiffView::new(Theme::dark())
            .unwrap()
            .render_review(&diff, &comments, &[]);
        let lines = "\
<#ebcb8b|#343d46|b>Review<#8a8a8a|#343d46|-> [press c here to draft the review comment]
<#8a8a8a|-|->  * <#8fa1b3|-|->wez (human)
<#c0c5ce|-|->    looks good overall
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,1 +1,1 @@
<#65737e|#2d3b30|->        1 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump(&doc.lines), lines.to_string());
    }

    #[test]
    fn a_line_comment_inside_a_long_run_splits_the_fold_around_it() {
        // A comment on a context line far from the change keeps that line and
        // its surrounding context, leaving a fold both above and below it rather
        // than hiding the commented line.
        let mut lines: Vec<(LineKind, String, u32)> = (1..=19)
            .map(|n| (LineKind::Context, format!("ctx{n:02}"), n))
            .collect();
        lines.push((LineKind::Added, "change!".to_string(), 20));
        let borrowed: Vec<(LineKind, &str, u32)> =
            lines.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect();
        let diff = Diff {
            files: vec![file("notes.txt", FileStatus::Modified, &borrowed)],
        };
        let comments = vec![comment(
            9,
            ("wez", AuthorKind::Human),
            on_lines("notes.txt", 6, 6),
            "here",
        )];
        let doc = DiffView::new(Theme::dark())
            .unwrap()
            .render_review(&diff, &comments, &[]);
        let markers: Vec<Line<'static>> = doc.folds.iter().map(|f| f.marker.clone()).collect();
        let expected = "\
<#8a8a8a|#343d46|->            [2 unchanged lines]  ctx02
<#8a8a8a|#343d46|->            [7 unchanged lines]  ctx16
";
        k9::assert_equal!(dump(&markers), expected.to_string());
    }

    #[test]
    fn an_outdated_comment_whose_line_is_gone_floats_to_the_file_header() {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Added, "let y = 2;", 1)],
            )],
        };
        // A comment anchored to line 40, which this file does not render.
        let mut outdated = comment(
            5,
            ("dev", AuthorKind::Human),
            on_lines("src/lib.rs", 40, 40),
            "stale",
        );
        outdated.confidence = Some(Confidence::Outdated);
        let doc = DiffView::new(Theme::dark())
            .unwrap()
            .render_review(&diff, &[outdated], &[]);
        let lines = "\
<#ebcb8b|#343d46|b>Review<#8a8a8a|#343d46|-> [press c here to draft the review comment]
<#c0c5ce|-|b>modified  src/lib.rs
<#8a8a8a|-|->  * <#8fa1b3|-|->dev (human)<#d08770|-|-> [outdated]
<#c0c5ce|-|->    stale
<#96b5b4|-|->@@ -1,1 +1,1 @@
<#65737e|#2d3b30|->        1 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump(&doc.lines), lines.to_string());
    }
}
