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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;
use ulid::Ulid;
use wiff_core::LineOrigin;
use wiff_core::record::{Author, CommentTarget, Disposition};
use wiff_core::review::{CommentState, threads};
use wiff_diff::{
    Diff, DiffLine, FileDiff, FileStatus, GeneratedMatchers, GeneratedReason, HighlightError,
    HighlightedLine, Highlighter, LineKind, LineNo, LiveHighlighter, ParsedSide, Parser, ReconLine,
    Rgb, Section, SectionMatchers, Side, StyledSpan, intraline, reconstitute,
};

use crate::action::Action;
use crate::filerender::FileRenderer;
use crate::keymap::Keymap;
use crate::markdown::{self, MarkdownColors};
use crate::theme::{Theme, legible_over};
use crate::wrap::wrap_line;

/// The gutter width for one side's line number.
const LINENO_WIDTH: usize = 4;

/// The width of the gutter before a content line: two line numbers, the change
/// marker, and the spaces separating them. A fold marker is indented this far so
/// it aligns under the code column.
const GUTTER_WIDTH: usize = LINENO_WIDTH * 2 + 4;

/// The width of one side-by-side column's gutter: a single line number, the
/// change marker, a leading space, and a trailing space that hosts the anchor
/// rail just left of the content.
pub(crate) const COLUMN_GUTTER_WIDTH: usize = LINENO_WIDTH + 3;

/// The single-cell rule drawn between the two side-by-side columns.
pub(crate) const COLUMN_DIVIDER: char = '\u{2502}';

/// The shortest run of unchanged lines worth collapsing. Shorter runs save too
/// few rows to justify a fold marker, and their context is usually worth
/// reading, so they are left expanded.
const MIN_FOLD: usize = 5;

/// The change-marker glyph for a collapsed fold, pointing right at the hidden
/// rows the way a closed disclosure triangle does.
const FOLD_COLLAPSED: char = '\u{25b8}';

/// The change-marker glyph on the first line of an expanded fold, pointing down
/// at the rows that would collapse behind it.
const FOLD_EXPANDED: char = '\u{25be}';

/// The change-marker glyph continuing down the remaining lines of an expanded
/// fold, tracing how far the collapsible region reaches.
const FOLD_BODY: char = '\u{2502}';

/// The anchor-rail glyph tracing a comment's line range down the last gutter
/// column, and the corner closing it on the range's final line.
const RAIL_BODY: char = '\u{2502}';
const RAIL_END: char = '\u{2514}';

/// The gutter column the anchor rail occupies: the last gutter cell, just left
/// of the content, where a comment box's bottom edge drops its tee.
pub(crate) const RAIL_COLUMN: usize = GUTTER_WIDTH - 1;

/// The tee joining a comment box's bottom edge down into the anchor rail.
pub(crate) const RAIL_TEE: char = '\u{252c}';

/// The anchor rail a content row draws in one gutter cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailCell {
    /// The character column the glyph occupies in the gutter.
    pub column: usize,
    /// The rail glyph: the body tracing the range down, or the corner closing it
    /// on the range's final line.
    pub glyph: char,
    /// The comment-box border color, matching the rail to its box.
    pub color: Rgb,
}

/// The character column the anchor rail for a comment on `side` occupies.
pub(crate) fn rail_column(mode: LayoutMode, side: Side, width: usize) -> usize {
    match mode {
        LayoutMode::Unified | LayoutMode::OnlyAfter | LayoutMode::Rendered => RAIL_COLUMN,
        LayoutMode::SideBySide => match side {
            Side::Before => COLUMN_GUTTER_WIDTH - 1,
            Side::After => ColumnGeometry::split(width).left + 1 + COLUMN_GUTTER_WIDTH - 1,
        },
    }
}

/// The character column of the divider rule between the two side-by-side
/// columns, in a viewport `width` columns wide.
pub(crate) fn divider_column(width: usize) -> usize {
    ColumnGeometry::split(width).left
}

/// The `(start_column, width)` of the `side` column, in a viewport `width`
/// columns wide.
pub(crate) fn column_bounds(width: usize, side: Side) -> (usize, usize) {
    let geo = ColumnGeometry::split(width);
    match side {
        Side::Before => (0, geo.left),
        Side::After => (geo.left + 1, geo.right),
    }
}

/// The file index for rows not tied to a file: the review summary and its
/// comments, which resolve to no path in the status line.
const NO_FILE: usize = usize::MAX;

/// The context lines kept on each side of a change when nothing overrides it.
pub const DEFAULT_DISPLAY_CONTEXT: usize = 3;

/// One rendered display row of a diff line: its styled line and the plain text
/// for search.
type ContentRow = (Line<'static>, String);

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
    /// The plain searchable text for each line, parallel to `lines`: a file's
    /// path, a content line's code, or a comment's author and body. Empty for
    /// rows that carry nothing worth matching, such as hunk headers and box
    /// edges.
    pub text: Vec<String>,
    /// The anchor rails each row draws, parallel to `lines`; empty for rows no
    /// comment range covers. Applied when comments are shown and dropped with
    /// them, since the rail is annotation chrome.
    pub rails: Vec<Vec<RailCell>>,
    /// The per-column text of each side-by-side content row, parallel to
    /// `lines`; `None` for unified and non-content rows.
    pub row_columns: Vec<Option<ColumnSplit>>,
    /// The column a side-by-side comment box row is scoped to, parallel to
    /// `lines`: `Some(side)` for a line comment's box drawn in that side's
    /// column, `None` for a full-width box and for non-box rows.
    pub box_columns: Vec<Option<Side>>,
    /// The foldable runs of unchanged rows, in row order, non-overlapping.
    pub folds: Vec<Fold>,
    /// The collapsible comment bodies, in row order.
    pub comments: Vec<CommentRegion>,
    /// The display path of each file, indexed by [`Row::file`].
    pub files: Vec<String>,
    /// The layout mode the document was rendered in; used by the draw pipeline to
    /// align per-column chrome.
    pub mode: LayoutMode,
}

/// The per-column visible text of a side-by-side content row.
pub struct ColumnSplit {
    /// The left (before) column's visible content.
    pub left: String,
    /// The right (after) column's visible content.
    pub right: String,
}

/// A rendered comment box: its top-edge header row and the body rows that
/// collapse behind it, closed off by a bottom-edge row that stays visible even
/// when the body is collapsed away.
pub struct CommentRegion {
    /// The box this region renders.
    pub id: BoxId,
    /// The row index of the comment's header line.
    pub header: usize,
    /// The body rows hidden when the comment is collapsed: `[start, end)`.
    pub body: Range<usize>,
    /// Whether the comment starts collapsed (resolved comments do).
    pub collapsed_default: bool,
    /// Whether the box anchors a line range, so its bottom edge drops the anchor
    /// rail into the gutter and the covered lines below trace it.
    pub anchor_rail: bool,
}

/// The concrete layout a render targets, resolved from a [`DiffMode`] and the
/// viewport width before rendering.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    /// One column, removed and added lines interleaved.
    #[default]
    Unified,
    /// Two columns, before on the left and after on the right.
    SideBySide,
    /// One column of the after side alone: context and added lines, with removed
    /// lines dropped so the column reads as the resulting file.
    OnlyAfter,
    /// One column of the after side rendered through a type-specific renderer.
    /// A file whose type has no renderer falls back to the
    /// [`OnlyAfter`](Self::OnlyAfter) source column.
    Rendered,
}

/// The default width, in columns, at or above which auto mode chooses the
/// side-by-side layout.
pub const DEFAULT_SIDE_BY_SIDE_MIN_WIDTH: usize = 130;

/// The diff layout a reviewer selects, resolved against the viewport width by
/// [`resolve`](Self::resolve).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffMode {
    /// Side-by-side when the viewport is wide enough, else unified.
    #[default]
    Auto,
    /// Always one interleaved column.
    Unified,
    /// Always two columns.
    SideBySide,
    /// Always the after side alone in one column.
    OnlyAfter,
    /// Always the after side rendered through a type-specific renderer, falling
    /// back to the after-side source column where no renderer fits the file.
    Rendered,
}

impl DiffMode {
    /// The concrete layout for a viewport `width` columns wide, taking
    /// side-by-side in auto mode only at or above `min_width`.
    pub fn resolve(self, width: usize, min_width: usize) -> LayoutMode {
        match self {
            DiffMode::Unified => LayoutMode::Unified,
            DiffMode::SideBySide => LayoutMode::SideBySide,
            DiffMode::OnlyAfter => LayoutMode::OnlyAfter,
            DiffMode::Rendered => LayoutMode::Rendered,
            DiffMode::Auto if width >= min_width => LayoutMode::SideBySide,
            DiffMode::Auto => LayoutMode::Unified,
        }
    }
}

/// The view width a render targets and whether the diff content wraps to it.
/// Comment bodies always wrap to the width; `wrap_content` additionally wraps
/// the diff lines instead of clipping them at the edge. A width of zero leaves
/// everything on one row, for use before a real width is known.
#[derive(Clone, Copy, Default)]
pub struct ViewLayout {
    /// The view width in columns.
    pub width: usize,
    /// Whether diff content lines wrap to the width rather than clip.
    pub wrap_content: bool,
    /// Whether the diff renders in one column or two.
    pub mode: LayoutMode,
}

/// The width of each side-by-side column, derived from a total view width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ColumnGeometry {
    left: usize,
    right: usize,
}

impl ColumnGeometry {
    /// Split `width` columns into two, reserving one cell for the divider. The
    /// right column takes the odd cell when the width does not halve evenly, so
    /// the new-content side is the wider one.
    fn split(width: usize) -> Self {
        let available = width.saturating_sub(1);
        let left = available / 2;
        Self {
            left,
            right: available - left,
        }
    }

    /// The content width of a column: the column less its gutter, or zero when
    /// the gutter alone already fills it.
    fn content_width(column: usize) -> usize {
        column.saturating_sub(COLUMN_GUTTER_WIDTH)
    }
}

/// The inputs to rendering one file: the file and its index, where its comments
/// are placed, how its content is colored, which comments show as uncommitted
/// drafts, and the layout its content and comment bodies fit.
#[derive(Clone, Copy)]
struct FileRender<'a> {
    index: usize,
    file: &'a FileDiff,
    placement: &'a FilePlacement<'a>,
    highlight: FileHighlight<'a>,
    pending: &'a [Ulid],
    layout: ViewLayout,
}

/// Where and how wide to draw a comment box, and whether it anchors a rail.
#[derive(Clone, Copy)]
struct BoxPlacement {
    /// The width to draw the comment box at.
    width: usize,
    /// The side-by-side column to scope the box to, or `None` for full width.
    column: Option<Side>,
    /// Whether the box anchors a line range, dropping a tee into the rail its
    /// covered lines trace.
    rail: bool,
}

/// The shared inputs for rendering a hunk's content rows, passed to both the
/// unified and side-by-side emission paths.
#[derive(Clone, Copy)]
struct HunkEmission<'a> {
    index: usize,
    hunk: &'a wiff_diff::Hunk,
    emphasis: &'a [Vec<Range<usize>>],
    before: &'a BTreeMap<LineNo, HighlightedLine>,
    after: &'a BTreeMap<LineNo, HighlightedLine>,
    fold_marks: &'a [Option<char>],
    placement: &'a FilePlacement<'a>,
    pending: &'a [Ulid],
    layout: ViewLayout,
}

/// Which highlight a file's content is painted from for a render.
#[derive(Clone, Copy)]
enum FileHighlight<'a> {
    /// Paint the file from this cached highlight.
    Ready(&'a FileHighlights),
    /// Paint the file plain: its highlight is still being computed.
    Plain,
    /// No cache is kept; highlight the file now, on the render.
    OnDemand,
}

/// A run of rows that can be collapsed behind a single marker line, either an
/// unchanged run within a hunk or a whole generated file's body.
pub struct Fold {
    /// The first hidden row index into [`Document::rows`].
    pub start: usize,
    /// One past the last hidden row index.
    pub end: usize,
    /// The line shown in place of the hidden rows when collapsed.
    pub marker: Line<'static>,
    /// The background the marker fills its whole row width with.
    pub fill: Option<Rgb>,
    /// Whether the fold starts collapsed. An unchanged run does; a generated
    /// file does too, unless it has comments that keep it open.
    pub collapsed_default: bool,
    /// Whether the fold hides a whole generated file rather than an unchanged
    /// run.
    pub whole_file: bool,
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
    /// A content line addressing a Before-side line in its left column and an
    /// After-side line in its right column, either of which may be blank.
    Content {
        /// The Before-side line, drawn in the left column.
        before: ColumnLine,
        /// The After-side line, drawn in the right column.
        after: ColumnLine,
    },
    /// The review summary row at the top of the document.
    ReviewSummary,
    /// A comment box's top edge, whose title names the author, status, and how
    /// to edit.
    CommentHeader {
        /// The box the header belongs to.
        id: BoxId,
    },
    /// One line of a comment's body, inside the box.
    CommentBody {
        /// The box the body belongs to.
        id: BoxId,
    },
    /// A comment box's bottom edge, closing the box below its body.
    CommentBottom {
        /// The box the bottom edge belongs to.
        id: BoxId,
    },
}

/// Which review-level box a comment row belongs to: an actual comment addressed
/// by its ulid, or the review's own description. Modeling the description as a
/// distinct variant rather than a reserved ulid forces every dispatch that acts
/// on a box to decide what the description does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoxId {
    /// A comment, addressed by its stable ulid.
    Comment(Ulid),
    /// The review description.
    Description,
}

/// One column's line on a content row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnLine {
    /// Blank filler: this column has no line on this row.
    Blank,
    /// A line in this column: its number, or `None` for a malformed line.
    Line(Option<LineNo>),
}

impl RowKind {
    /// Build a content row addressing a single line on `side`, leaving the
    /// opposite column blank. Unified rows and the filler side of a side-by-side
    /// row are built this way.
    pub fn content(side: Side, lineno: Option<LineNo>) -> Self {
        match side {
            Side::Before => RowKind::Content {
                before: ColumnLine::Line(lineno),
                after: ColumnLine::Blank,
            },
            Side::After => RowKind::Content {
                before: ColumnLine::Blank,
                after: ColumnLine::Line(lineno),
            },
        }
    }

    /// The `(side, lineno)` a content row is addressed by, applying the
    /// comment-side rule: the After column when it holds a numbered line, else
    /// the Before column. `None` for a non-content row or one whose addressed
    /// column holds no number.
    pub fn content_addr(&self) -> Option<(Side, LineNo)> {
        let RowKind::Content { before, after } = self else {
            return None;
        };
        if let ColumnLine::Line(Some(n)) = after {
            return Some((Side::After, *n));
        }
        if let ColumnLine::Line(Some(n)) = before {
            return Some((Side::Before, *n));
        }
        None
    }

    /// Whether this is a content row, numbered or malformed, in either column.
    pub fn is_content(&self) -> bool {
        matches!(self, RowKind::Content { .. })
    }
}

impl Document {
    /// Append a styled line, the background it fills its row with, and its
    /// parallel row metadata.
    fn push(
        &mut self,
        file: usize,
        kind: RowKind,
        fill: Option<Rgb>,
        text: String,
        line: Line<'static>,
    ) {
        self.lines.push(line);
        self.fills.push(fill);
        self.rows.push(Row { file, kind });
        self.text.push(text);
        self.rails.push(Vec::new());
        self.row_columns.push(None);
        self.box_columns.push(None);
    }

    /// Append a side-by-side content row, recording its two column text runs so
    /// the search wash can find a match within the column it falls in.
    fn push_columns(
        &mut self,
        file: usize,
        kind: RowKind,
        text: String,
        line: Line<'static>,
        columns: ColumnSplit,
    ) {
        self.push(file, kind, None, text, line);
        if let Some(slot) = self.row_columns.last_mut() {
            *slot = Some(columns);
        }
    }
}

/// A file's highlighted content on both sides, indexed by line number, so a
/// render can look up each row's colored spans without re-running the syntax
/// highlighter. Produced by [`DiffView::recolor`] and reused across renders of
/// the same diff.
pub struct FileHighlights {
    before: BTreeMap<LineNo, HighlightedLine>,
    after: BTreeMap<LineNo, HighlightedLine>,
}

/// A file's content parsed on both sides, the costly, theme-independent part of
/// highlighting kept so a theme change recolors without parsing again. Produced
/// by [`DiffView::parse`] and colored into a [`FileHighlights`] per theme.
pub struct ParsedFile {
    before: ParsedSide,
    after: ParsedSide,
}

impl ParsedFile {
    /// Assemble a parsed file from its two already-parsed sides.
    pub fn from_sides(before: ParsedSide, after: ParsedSide) -> Self {
        Self { before, after }
    }
}

/// A renderer pairing a syntax highlighter with a color theme.
pub struct DiffView {
    highlighter: Highlighter,
    theme: Theme,
    display_context: usize,
    sections: SectionMatchers,
    generated: GeneratedMatchers,
    hints: KeyHints,
}

/// The key labels the review view names in its hints, resolved from the active
/// keymap so each hint shows the reviewer's own binding.
pub struct KeyHints {
    /// The key that drafts a new comment.
    pub add_comment: String,
    /// The key that replies to the focused comment.
    pub reply_comment: String,
    /// The key that edits the focused comment.
    pub edit_comment: String,
    /// The key that resolves or reopens the focused comment.
    pub resolve_comment: String,
    /// The key that deletes or restores the focused comment.
    pub delete_comment: String,
    /// The key that expands or collapses the focused comment.
    pub toggle_comment: String,
}

impl KeyHints {
    /// Resolve the review hints from `keymap`, naming each comment action's
    /// first bound chord. An action the keymap leaves unbound keeps its
    /// built-in default label.
    pub fn from_keymap(keymap: &Keymap) -> Self {
        let default = Self::default();
        Self {
            add_comment: keymap
                .primary_label(Action::AddComment)
                .unwrap_or(default.add_comment),
            reply_comment: keymap
                .primary_label(Action::ReplyComment)
                .unwrap_or(default.reply_comment),
            edit_comment: keymap
                .primary_label(Action::EditComment)
                .unwrap_or(default.edit_comment),
            resolve_comment: keymap
                .primary_label(Action::ResolveComment)
                .unwrap_or(default.resolve_comment),
            delete_comment: keymap
                .primary_label(Action::DeleteComment)
                .unwrap_or(default.delete_comment),
            toggle_comment: keymap
                .primary_label(Action::ToggleComment)
                .unwrap_or(default.toggle_comment),
        }
    }
}

impl Default for KeyHints {
    /// The labels the built-in keymap resolves the comment actions to, so the
    /// hints stay in step with the default bindings without repeating them.
    fn default() -> Self {
        let keymap = Keymap::defaults();
        Self {
            add_comment: keymap.primary_label(Action::AddComment).unwrap_or_default(),
            reply_comment: keymap
                .primary_label(Action::ReplyComment)
                .unwrap_or_default(),
            edit_comment: keymap
                .primary_label(Action::EditComment)
                .unwrap_or_default(),
            resolve_comment: keymap
                .primary_label(Action::ResolveComment)
                .unwrap_or_default(),
            delete_comment: keymap
                .primary_label(Action::DeleteComment)
                .unwrap_or_default(),
            toggle_comment: keymap
                .primary_label(Action::ToggleComment)
                .unwrap_or_default(),
        }
    }
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
            generated: GeneratedMatchers::builtins(),
            hints: KeyHints::default(),
        })
    }

    /// Recolor the renderer to `theme`, swapping only the syntax highlighter's
    /// color mapping and keeping the loaded syntaxes, display context, section
    /// and generated-file matchers, and key hints already configured. Reusing a [`parse`](Self::parse)
    /// of the diff, this keeps a theme change off the costly syntax parse. On an
    /// unknown syntax theme the renderer is left unchanged.
    pub fn set_theme(&mut self, theme: Theme) -> Result<(), HighlightError> {
        self.highlighter.set_theme(&theme.syntax_theme)?;
        self.theme = theme;
        Ok(())
    }

    /// Name the reviewer's own bindings in the review hints, so the review
    /// summary and each comment box show the keys their keymap resolves the
    /// comment actions to.
    pub fn with_key_hints(mut self, hints: KeyHints) -> Self {
        self.hints = hints;
        self
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

    /// Recognise machine-generated files with `generated`, so each such file
    /// shows a badge and folds to its header by default.
    pub fn with_generated_file_matches(mut self, generated: GeneratedMatchers) -> Self {
        self.generated = generated;
        self
    }

    /// Render every file of `diff` into one scrollable [`Document`], with no
    /// review overlay.
    pub fn render(&self, diff: &Diff) -> Document {
        self.build(
            diff,
            ReviewInputs {
                comments: &[],
                pending: &[],
                origins: &CommentOrigins::Literal,
                description: None,
                describe_hint: false,
            },
            false,
            None,
            ViewLayout::default(),
        )
    }

    /// Parse every file in `diff` into its scope operations, once, so a theme
    /// change recolors without parsing again. The parse is the costly part of
    /// highlighting and the diff content is fixed for the life of a capture, so
    /// a review parses on capture and reuses it until the next refresh.
    pub fn parse(&self, diff: &Diff) -> Vec<ParsedFile> {
        diff.files
            .iter()
            .map(|file| ParsedFile {
                before: self.highlighter.parse_side(file, Side::Before),
                after: self.highlighter.parse_side(file, Side::After),
            })
            .collect()
    }

    /// Color a `parsed` diff with the current theme, keyed by line, so the
    /// result can be reused across re-renders that only change comments. This is
    /// the cheap part of highlighting: a theme change replays the cached parse
    /// through the new theme rather than parsing the diff again.
    pub fn recolor(&self, parsed: &[ParsedFile]) -> Vec<FileHighlights> {
        parsed.iter().map(|file| self.recolor_file(file)).collect()
    }

    /// Color one `parsed` file with the current theme, for folding a single
    /// background parse result into the highlight cache as it arrives.
    pub fn recolor_file(&self, parsed: &ParsedFile) -> FileHighlights {
        FileHighlights {
            before: self.highlighter.color_side(&parsed.before),
            after: self.highlighter.color_side(&parsed.after),
        }
    }

    /// Return a [`Parser`] that shares this view's syntaxes, for parsing files
    /// off the main thread while the view stays put to color the results.
    pub fn parser(&self) -> Parser {
        self.highlighter.parser()
    }

    /// An incremental highlighter for `token`'s syntax under this view's current
    /// theme, for coloring the inline editor as it is typed.
    pub fn live_highlighter(&self, token: &str) -> LiveHighlighter {
        self.highlighter.live(token)
    }

    /// Render `diff` with `comments` woven in: a review summary row at the top,
    /// whole-file comments under their file header, and line comments in a block
    /// above the line they anchor. Deleted comments are the caller's to filter.
    /// Comments whose id is in `pending` are badged as uncommitted drafts. Long
    /// comment bodies wrap to fit `layout`; the diff content wraps too when
    /// `layout.wrap_content` is set, otherwise it is clipped at draw.
    pub fn render_review(
        &self,
        diff: &Diff,
        comments: &[CommentState],
        pending: &[Ulid],
        layout: ViewLayout,
    ) -> Document {
        self.build(
            diff,
            ReviewInputs {
                comments,
                pending,
                origins: &CommentOrigins::Literal,
                description: None,
                describe_hint: false,
            },
            true,
            None,
            layout,
        )
    }

    /// Render using `highlights` from an earlier [`recolor`](Self::recolor) of
    /// the same diff. A file whose highlight is not yet available renders plain
    /// until it arrives.
    pub fn render_review_cached(
        &self,
        diff: &Diff,
        comments: &[CommentState],
        pending: &[Ulid],
        highlights: &[Option<FileHighlights>],
        layout: ViewLayout,
    ) -> Document {
        self.build(
            diff,
            ReviewInputs {
                comments,
                pending,
                origins: &CommentOrigins::Literal,
                description: None,
                describe_hint: false,
            },
            true,
            Some(highlights),
            layout,
        )
    }

    /// Like [`render_review_cached`](Self::render_review_cached), but placing
    /// each line comment by the version and side its content truly belongs to,
    /// per `origins`. This is how a comparison view keeps a comment on the line
    /// it was authored against even though the presented before side is an
    /// earlier version's content rather than the latest version's before side.
    pub(crate) fn render_review_origins(
        &self,
        diff: &Diff,
        inputs: ReviewInputs,
        highlights: &[Option<FileHighlights>],
        layout: ViewLayout,
    ) -> Document {
        self.build(diff, inputs, true, Some(highlights), layout)
    }

    /// The shared render path: build the document, optionally leading with the
    /// review summary row, then each file with its placed comments.
    fn build(
        &self,
        diff: &Diff,
        inputs: ReviewInputs,
        review_row: bool,
        highlights: Option<&[Option<FileHighlights>]>,
        layout: ViewLayout,
    ) -> Document {
        let ReviewInputs {
            comments,
            pending,
            origins,
            description,
            describe_hint,
        } = inputs;
        let mut doc = Document {
            lines: Vec::new(),
            rails: Vec::new(),
            row_columns: Vec::new(),
            box_columns: Vec::new(),
            fills: Vec::new(),
            rows: Vec::new(),
            text: Vec::new(),
            folds: Vec::new(),
            comments: Vec::new(),
            files: diff
                .files
                .iter()
                .map(|f| f.display_path().to_string())
                .collect(),
            mode: layout.mode,
        };
        let placement = Placement::new(diff, comments, origins);
        if review_row {
            doc.push(
                NO_FILE,
                RowKind::ReviewSummary,
                Some(self.theme.status_bg),
                String::new(),
                self.review_summary(describe_hint),
            );
            if let Some(DescriptionBox { comment, pending }) = description {
                self.push_comment(
                    &mut doc,
                    NO_FILE,
                    comment,
                    BoxId::Description,
                    pending,
                    BoxPlacement {
                        width: layout.width,
                        column: None,
                        rail: false,
                    },
                );
            }
            for placed in &placement.review {
                self.push_thread(
                    &mut doc,
                    NO_FILE,
                    placed,
                    pending,
                    BoxPlacement {
                        width: layout.width,
                        column: None,
                        rail: false,
                    },
                );
            }
        }
        for (index, file) in diff.files.iter().enumerate() {
            let highlight = match highlights {
                None => FileHighlight::OnDemand,
                Some(cached) => match cached[index].as_ref() {
                    Some(ready) => FileHighlight::Ready(ready),
                    None => FileHighlight::Plain,
                },
            };
            self.render_file(
                &mut doc,
                FileRender {
                    index,
                    file,
                    placement: &placement.files[index],
                    highlight,
                    pending,
                    layout,
                },
            );
        }
        doc
    }

    /// Append `file`'s header, its whole-file and floated comments, and its
    /// hunks with any line comments woven in.
    fn render_file(&self, doc: &mut Document, render: FileRender) {
        let FileRender {
            index,
            file,
            placement,
            highlight,
            pending,
            layout,
        } = render;
        let width = layout.width;
        let generated = self.generated.classify(file);
        let content_lines = generated_fold_line_count(file);
        doc.push(
            index,
            RowKind::FileHeader,
            None,
            file.display_path().to_string(),
            self.file_header(file, generated.is_some()),
        );
        for placed in &placement.header {
            self.push_thread(
                doc,
                index,
                placed,
                pending,
                BoxPlacement {
                    width,
                    column: None,
                    rail: false,
                },
            );
        }
        // A generated file folds everything below its whole-file comments: the
        // hunk body and any line comments woven into it. The whole-file comments
        // stay above the fold so they remain visible while it is collapsed.
        let body_start = doc.rows.len();
        if layout.mode == LayoutMode::Rendered
            && let Some(rows) = self.render_after_content(file, width)
        {
            self.emit_rendered(doc, index, placement, pending, &rows, layout);
            self.push_whole_file_fold(doc, generated, body_start, content_lines, placement);
            return;
        }
        // Paint from the cached highlight when it has arrived; render plain
        // while it is still being computed; or, when no cache is kept, run the
        // syntect pass for this file now.
        let computed;
        let (before, after) = match highlight {
            FileHighlight::Ready(cached) => (&cached.before, &cached.after),
            FileHighlight::OnDemand => {
                computed = FileHighlights {
                    before: self.highlighter.highlight_side(file, Side::Before),
                    after: self.highlighter.highlight_side(file, Side::After),
                };
                (&computed.before, &computed.after)
            }
            FileHighlight::Plain => {
                computed = FileHighlights {
                    before: BTreeMap::new(),
                    after: BTreeMap::new(),
                };
                (&computed.before, &computed.after)
            }
        };
        // The first display row of each content line, keyed by side and number,
        // so the rail post-pass can walk a comment's line range in display order.
        let mut first_row: HashMap<(Side, u32), usize> = HashMap::new();
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            doc.push(
                index,
                RowKind::HunkHeader { hunk: hunk_index },
                None,
                String::new(),
                self.hunk_header(hunk),
            );
            let emphasis = intraline::refine(&hunk.lines);
            // A wrapped content line spans several rows, so a fold needs the row
            // one past a line's last row, not one past its first; track both,
            // indexed by hunk line so a fold run's boundaries resolve directly.
            let mut line_row = vec![0usize; hunk.lines.len()];
            let mut line_end = vec![0usize; hunk.lines.len()];
            // Work out the foldable runs up front so each context line inside
            // one can show a fold-column glyph, marking the region as
            // collapsible even while it is expanded.
            let kinds: Vec<LineKind> = hunk.lines.iter().map(|line| line.kind).collect();
            let anchored: Vec<bool> = hunk
                .lines
                .iter()
                .map(|line| line_anchor(line).is_some_and(|(s, n)| placement.covers(s, n)))
                .collect();
            // A generated file folds as one unit, so its unchanged runs are not
            // folded separately and show no fold-column glyph.
            let runs = if generated.is_some() {
                Vec::new()
            } else {
                foldable_runs(&kinds, self.display_context, &anchored)
            };
            let fold_marks = fold_column(&runs, hunk.lines.len());
            let emission = HunkEmission {
                index,
                hunk,
                emphasis: &emphasis,
                before,
                after,
                fold_marks: &fold_marks,
                placement,
                pending,
                layout,
            };
            match layout.mode {
                LayoutMode::Unified => self.emit_hunk_unified(
                    doc,
                    &emission,
                    false,
                    &mut line_row,
                    &mut line_end,
                    &mut first_row,
                ),
                LayoutMode::OnlyAfter | LayoutMode::Rendered => self.emit_hunk_unified(
                    doc,
                    &emission,
                    true,
                    &mut line_row,
                    &mut line_end,
                    &mut first_row,
                ),
                LayoutMode::SideBySide => self.emit_hunk_columns(
                    doc,
                    &emission,
                    &mut line_row,
                    &mut line_end,
                    &mut first_row,
                ),
            }
            let section = self.sections.for_path(file.display_path());
            for run in runs {
                let scope = enclosing_scope(&hunk.lines, run.end, &section);
                doc.folds.push(Fold {
                    start: line_row[run.start],
                    end: line_end[run.end - 1],
                    marker: self.fold_marker(run.end - run.start, scope),
                    fill: None,
                    collapsed_default: true,
                    whole_file: false,
                });
            }
        }
        self.trace_rails(doc, placement, pending, &first_row, layout);
        self.push_whole_file_fold(doc, generated, body_start, content_lines, placement);
    }

    /// Fold a generated file's body, from `body_start` to the last row emitted,
    /// behind its header, labeling the marker with `content_lines`. The fold
    /// starts collapsed unless the body holds a line comment, which keeps it
    /// open. Does nothing for a file `generated` did not recognise, or one whose
    /// body is empty.
    fn push_whole_file_fold(
        &self,
        doc: &mut Document,
        generated: Option<GeneratedReason>,
        body_start: usize,
        content_lines: usize,
        placement: &FilePlacement,
    ) {
        if generated.is_none() {
            return;
        }
        let end = doc.rows.len();
        if end <= body_start {
            return;
        }
        let plural = if content_lines == 1 { "" } else { "s" };
        doc.folds.push(Fold {
            start: body_start,
            end,
            marker: self.fold_marker_line(&format!("{content_lines} line{plural}"), None),
            fill: None,
            collapsed_default: placement.lines.is_empty(),
            whole_file: true,
        });
    }

    /// Emit one hunk's content rows in the unified layout: each diff line becomes
    /// its own display rows, with any line comments woven in above it. With
    /// `only_after`, removed lines contribute no content row, leaving the after
    /// side alone; a comment anchored to a removed line still opens, at the point
    /// the removal falls, though without a line to trace its rail down.
    fn emit_hunk_unified(
        &self,
        doc: &mut Document,
        e: &HunkEmission,
        only_after: bool,
        line_row: &mut [usize],
        line_end: &mut [usize],
        first_row: &mut HashMap<(Side, u32), usize>,
    ) {
        let width = e.layout.width;
        let content_wrap = if e.layout.wrap_content && width > GUTTER_WIDTH {
            Some(width - GUTTER_WIDTH)
        } else {
            None
        };
        for (line_index, (line, ranges)) in e.hunk.lines.iter().zip(e.emphasis).enumerate() {
            let (side, lineno, highlighted) = match line.kind {
                LineKind::Removed => (
                    Side::Before,
                    line.old_lineno,
                    line.old_lineno.and_then(|n| e.before.get(&n)),
                ),
                LineKind::Context | LineKind::Added => (
                    Side::After,
                    line.new_lineno,
                    line.new_lineno.and_then(|n| e.after.get(&n)),
                ),
            };
            if let Some(n) = lineno {
                for lc in e.placement.at(side, n.get()) {
                    self.push_thread(
                        doc,
                        e.index,
                        &lc.placed,
                        e.pending,
                        BoxPlacement {
                            width,
                            column: None,
                            rail: true,
                        },
                    );
                }
            }
            line_row[line_index] = doc.rows.len();
            if only_after && line.kind == LineKind::Removed {
                line_end[line_index] = doc.rows.len();
                continue;
            }
            // Remember the first display row of each content line, keyed by side
            // and number, so the rail post-pass can walk a comment's covered
            // span in document order.
            if let Some(n) = lineno {
                first_row.entry((side, n.get())).or_insert(doc.rows.len());
            }
            let (fill, rows) = self.content_rows(
                line,
                highlighted,
                ranges,
                content_wrap,
                e.fold_marks[line_index],
            );
            for (rendered, text) in rows {
                doc.push(
                    e.index,
                    RowKind::content(side, lineno),
                    fill,
                    text,
                    rendered,
                );
            }
            line_end[line_index] = doc.rows.len();
        }
    }

    /// Render `file`'s after-side content through the renderer its type selects,
    /// pairing each display row with the source line it derives from. Returns
    /// `None` when no renderer fits the file or its after side cannot be fully
    /// reconstructed, in which case the caller shows the source column instead.
    fn render_after_content(
        &self,
        file: &FileDiff,
        width: usize,
    ) -> Option<Vec<(Line<'static>, LineNo)>> {
        let renderer = FileRenderer::for_path(file.display_path())?;
        let text = after_side_text(file)?;
        let content_width = width.saturating_sub(GUTTER_WIDTH).max(1);
        let mapped = match renderer {
            FileRenderer::Markdown => {
                let colors = MarkdownColors::from_theme(&self.theme);
                markdown::render_source_mapped(&text, content_width, &colors, &self.highlighter)
            }
        };
        Some(
            mapped
                .into_iter()
                .filter_map(|(line, n)| LineNo::new(n).map(|n| (line, n)))
                .collect(),
        )
    }

    /// Emit a file's rendered after-side content as one column: each row leads
    /// with a line-number gutter, numbered once where the source line changes,
    /// and a comment anchored to a rendered source line opens above that line's
    /// first row. A line comment whose anchor has no rendered row (a before-side
    /// line, or a source line the renderer drops) floats to the file header
    /// instead of being lost.
    fn emit_rendered(
        &self,
        doc: &mut Document,
        index: usize,
        placement: &FilePlacement,
        pending: &[Ulid],
        rows: &[(Line<'static>, LineNo)],
        layout: ViewLayout,
    ) {
        let width = layout.width;
        // The after-side source lines that reach a visible row; a comment on any
        // other line has nowhere to open inline and floats to the header.
        let rendered_lines: HashSet<u32> = rows
            .iter()
            .filter(|(line, _)| !line.spans.iter().all(|s| s.content.trim().is_empty()))
            .map(|(_, source)| source.get())
            .collect();
        for lc in &placement.lines {
            if lc.side != Side::After || !rendered_lines.contains(&lc.start) {
                self.push_thread(
                    doc,
                    index,
                    &lc.placed,
                    pending,
                    BoxPlacement {
                        width,
                        column: None,
                        rail: false,
                    },
                );
            }
        }
        let mut first_row: HashMap<(Side, u32), usize> = HashMap::new();
        let mut prev_line: Option<u32> = None;
        for (line, source) in rows {
            let n = source.get();
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            // A blank separator between blocks shows no number and no comment;
            // both go to the first row of the source line that shows content.
            let renderable = !text.trim().is_empty();
            let first_of_line = renderable && prev_line != Some(n);
            if first_of_line {
                for lc in placement.at(Side::After, n) {
                    self.push_thread(
                        doc,
                        index,
                        &lc.placed,
                        pending,
                        BoxPlacement {
                            width,
                            column: None,
                            rail: true,
                        },
                    );
                }
            }
            if renderable {
                first_row.entry((Side::After, n)).or_insert(doc.rows.len());
                prev_line = Some(n);
            }
            let shown = if first_of_line { Some(*source) } else { None };
            let gutter = Span::styled(
                format!("{} {} {} ", lineno(None), lineno(shown), ' '),
                Style::default().fg(color(self.theme.gutter_fg)),
            );
            let mut spans = vec![gutter];
            spans.extend(line.spans.iter().cloned());
            doc.push(
                index,
                RowKind::content(Side::After, Some(*source)),
                None,
                text,
                Line::from(spans),
            );
        }
        self.trace_rails(doc, placement, pending, &first_row, layout);
    }

    /// Emit one hunk's content rows in the side-by-side layout: removed and added
    /// lines pair across the two columns, a context line shows on both, and a
    /// line comment opens in the column of the side it anchors to.
    fn emit_hunk_columns(
        &self,
        doc: &mut Document,
        e: &HunkEmission,
        line_row: &mut [usize],
        line_end: &mut [usize],
        first_row: &mut HashMap<(Side, u32), usize>,
    ) {
        let geo = ColumnGeometry::split(e.layout.width);
        let wrap = e.layout.wrap_content;
        let divider = Span::styled(
            COLUMN_DIVIDER.to_string(),
            Style::default().fg(color(self.theme.gutter_fg)),
        );
        for pair in pair_hunk_lines(&e.hunk.lines) {
            self.push_column_comments(doc, e, pair.left, Side::Before);
            self.push_column_comments(doc, e, pair.right, Side::After);

            let left_rows = self.column_side(e, pair.left, Side::Before, geo.left, wrap);
            let right_rows = self.column_side(e, pair.right, Side::After, geo.right, wrap);
            let height = left_rows.len().max(right_rows.len()).max(1);

            let start = doc.rows.len();
            if let Some(i) = pair.left {
                line_row[i] = start;
                if let Some(n) = e.hunk.lines[i].old_lineno {
                    first_row.entry((Side::Before, n.get())).or_insert(start);
                }
            }
            if let Some(i) = pair.right {
                line_row[i] = start;
                if let Some(n) = e.hunk.lines[i].new_lineno {
                    first_row.entry((Side::After, n.get())).or_insert(start);
                }
            }
            let before = match pair.left {
                Some(i) => ColumnLine::Line(e.hunk.lines[i].old_lineno),
                None => ColumnLine::Blank,
            };
            let after = match pair.right {
                Some(i) => ColumnLine::Line(e.hunk.lines[i].new_lineno),
                None => ColumnLine::Blank,
            };
            let kind = RowKind::Content { before, after };
            for r in 0..height {
                let (mut spans, left_text) = match left_rows.get(r) {
                    Some((s, t)) => (s.clone(), t.clone()),
                    None => (blank_column(geo.left), String::new()),
                };
                let (right_spans, right_text) = match right_rows.get(r) {
                    Some((s, t)) => (s.clone(), t.clone()),
                    None => (blank_column(geo.right), String::new()),
                };
                spans.push(divider.clone());
                spans.extend(right_spans);
                doc.push_columns(
                    e.index,
                    kind.clone(),
                    join_column_text(&left_text, &right_text),
                    Line::from(spans),
                    ColumnSplit {
                        left: left_text,
                        right: right_text,
                    },
                );
            }
            let end = doc.rows.len();
            if let Some(i) = pair.left {
                line_end[i] = end;
            }
            if let Some(i) = pair.right {
                line_end[i] = end;
            }
        }
    }

    /// Push the comments anchored to the `side` line at hunk index `idx`, above
    /// its row, in that side's column. A blank column or an unnumbered line has
    /// none.
    fn push_column_comments(
        &self,
        doc: &mut Document,
        e: &HunkEmission,
        idx: Option<usize>,
        side: Side,
    ) {
        let Some(i) = idx else { return };
        let number = match side {
            Side::Before => e.hunk.lines[i].old_lineno,
            Side::After => e.hunk.lines[i].new_lineno,
        };
        let Some(n) = number else { return };
        let (_, col_width) = column_bounds(e.layout.width, side);
        for lc in e.placement.at(side, n.get()) {
            self.push_thread(
                doc,
                e.index,
                &lc.placed,
                e.pending,
                BoxPlacement {
                    width: col_width,
                    column: Some(side),
                    rail: true,
                },
            );
        }
    }

    /// Render the `side` column of a logical row `col_width` columns wide: the
    /// display rows of the line at hunk index `idx`, or no rows when the column
    /// is blank filler. Each row is a one-number gutter, the syntax-colored
    /// content bounded to the column (wrapped when `wrap`, else clipped), and
    /// background padding to the column's right edge so the row tint reaches the
    /// divider. Returns each display row's spans and its plain text.
    fn column_side(
        &self,
        e: &HunkEmission,
        idx: Option<usize>,
        side: Side,
        col_width: usize,
        wrap: bool,
    ) -> Vec<(Vec<Span<'static>>, String)> {
        let Some(i) = idx else { return Vec::new() };
        let line = &e.hunk.lines[i];
        let (number, map) = match side {
            Side::Before => (line.old_lineno, e.before),
            Side::After => (line.new_lineno, e.after),
        };
        let highlighted = number.and_then(|n| map.get(&n));
        let StyledLine {
            marker,
            row_bg,
            gutter_style,
            content,
        } = self.style_line(line, highlighted, &e.emphasis[i], e.fold_marks[i]);
        let gutter = Span::styled(format!("{} {} ", lineno(number), marker), gutter_style);
        let blank_gutter = Span::styled(" ".repeat(COLUMN_GUTTER_WIDTH), gutter_style);
        let content_width = ColumnGeometry::content_width(col_width);
        let content = Line::from(content);
        let visual = if wrap {
            wrap_line(&content, content_width)
        } else {
            vec![clip_line(&content, content_width)]
        };
        visual
            .into_iter()
            .enumerate()
            .map(|(row, visual)| {
                let text: String = visual.spans.iter().map(|s| s.content.as_ref()).collect();
                let lead = if row == 0 {
                    gutter.clone()
                } else {
                    blank_gutter.clone()
                };
                let mut spans = vec![lead];
                spans.extend(visual.spans);
                let used = text.chars().count();
                if content_width > used {
                    spans.push(pad_span(content_width - used, row_bg));
                }
                (spans, text)
            })
            .collect()
    }

    /// Draw each line comment's anchor rail into `doc.rails`. A comment's rail
    /// runs from the first display row of its start line down to the first
    /// display row of its end line, closing with the corner there; every row
    /// between traces the body glyph, including the opposite-side rows a unified
    /// diff interleaves, so the rail reads as one unbroken stroke down the
    /// gutter. Each rail sits in the gutter of the side its comment anchors to,
    /// so a side-by-side row may trace one down each column. Where two comments
    /// overlap the same gutter column, the one reaching deepest wins the row: its
    /// color and, at its last line, the closing corner.
    fn trace_rails(
        &self,
        doc: &mut Document,
        placement: &FilePlacement,
        pending: &[Ulid],
        first_row: &HashMap<(Side, u32), usize>,
        layout: ViewLayout,
    ) {
        // The deepest closing row recorded for each railed (row, column), so a
        // shallower comment does not overwrite a deeper one sharing the column.
        let mut reach: HashMap<(usize, usize), usize> = HashMap::new();
        for lc in &placement.lines {
            let (Some(&start_row), Some(&end_row)) = (
                first_row.get(&(lc.side, lc.start)),
                first_row.get(&(lc.side, lc.end)),
            ) else {
                continue;
            };
            let color = self.comment_border(pending.contains(&lc.placed.comment.id));
            let column = rail_column(layout.mode, lc.side, layout.width);
            for row in start_row..=end_row {
                if !matches!(doc.rows[row].kind, RowKind::Content { .. }) {
                    continue;
                }
                if reach
                    .get(&(row, column))
                    .is_some_and(|&deepest| deepest >= end_row)
                {
                    continue;
                }
                reach.insert((row, column), end_row);
                let glyph = if row == end_row { RAIL_END } else { RAIL_BODY };
                let cell = RailCell {
                    column,
                    glyph,
                    color,
                };
                let rails = &mut doc.rails[row];
                rails.retain(|c| c.column != column);
                rails.push(cell);
            }
        }
    }

    /// The review summary row heading, the top-of-document target for review
    /// comments and the jump-to-top landing spot, backed by the status bar color
    /// with a dimmed hint at how to draft a review-level comment. When
    /// `describe_hint` is set the review has no description yet, so the row also
    /// offers the key that writes the first one.
    fn review_summary(&self, describe_hint: bool) -> Line<'static> {
        let bg = color(self.theme.status_bg);
        let hint_fg = color(legible_over(
            self.theme.fold_fg,
            self.theme.status_bg,
            self.theme.background,
        ));
        let mut spans = vec![
            Span::styled(
                "Review",
                Style::default()
                    .fg(color(self.theme.review_fg))
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " [press {} here to draft the review comment]",
                    self.hints.add_comment
                ),
                Style::default().fg(hint_fg).bg(bg),
            ),
        ];
        if describe_hint {
            spans.push(Span::styled(
                format!(
                    " [press {} to write the description]",
                    self.hints.edit_comment
                ),
                Style::default().fg(hint_fg).bg(bg),
            ));
        }
        Line::from(spans)
    }

    /// Push a root comment box and the reply boxes threaded beneath it, in
    /// order. Each reply renders in the root's column and width without an
    /// anchor rail of its own; the root keeps the rail its placement asks for.
    fn push_thread(
        &self,
        doc: &mut Document,
        file: usize,
        placed: &PlacedComment,
        pending: &[Ulid],
        placement: BoxPlacement,
    ) {
        self.push_comment(
            doc,
            file,
            placed.comment,
            BoxId::Comment(placed.comment.id),
            pending.contains(&placed.comment.id),
            placement,
        );
        for reply in &placed.replies {
            self.push_comment(
                doc,
                file,
                reply,
                BoxId::Comment(reply.id),
                pending.contains(&reply.id),
                BoxPlacement {
                    rail: false,
                    ..placement
                },
            );
        }
    }

    /// Append `comment` as a box: a top-edge header row, its body rows, and a
    /// bottom-edge row, all sharing the box border color. Each of the body's own
    /// lines is wrapped to fit the box interior at `width`, so a long line an
    /// agent writes on one row spreads across several rows the reviewer can read
    /// without scrolling sideways. The header and bottom rows stay visible when
    /// the body collapses, so a folded comment still reads as a closed box. A
    /// `column` scopes the box to one side-by-side column, `width` columns wide;
    /// `None` draws it full width. Records the collapsible body range so a
    /// resolved comment starts collapsed.
    fn push_comment(
        &self,
        doc: &mut Document,
        file: usize,
        comment: &CommentState,
        id: BoxId,
        pending: bool,
        placement: BoxPlacement,
    ) {
        let BoxPlacement {
            width,
            column,
            rail,
        } = placement;
        let border = self.comment_border(pending);
        let header = doc.rows.len();
        doc.push(
            file,
            RowKind::CommentHeader { id },
            Some(border),
            self.header_search_text(comment, id),
            self.comment_title(comment, id, pending),
        );
        let body_start = doc.rows.len();
        // Compensate for the border drawn around the comment box: its two
        // columns leave the body this much room to render into.
        let interior = width.saturating_sub(2);
        let colors = MarkdownColors::from_theme(&self.theme);
        for line in markdown::render(
            comment.body.trim_end(),
            interior,
            &colors,
            &self.highlighter,
        ) {
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            doc.push(file, RowKind::CommentBody { id }, Some(border), plain, line);
        }
        let body_end = doc.rows.len();
        doc.push(
            file,
            RowKind::CommentBottom { id },
            Some(border),
            String::new(),
            Line::default(),
        );
        doc.comments.push(CommentRegion {
            id,
            header,
            body: body_start..body_end,
            collapsed_default: comment.resolved || comment.deleted,
            anchor_rail: rail,
        });
        // A side-by-side line comment's box is scoped to its column; record the
        // column on each of the box's rows so the draw pipeline places it there.
        if column.is_some() {
            for slot in &mut doc.box_columns[header..doc.rows.len()] {
                *slot = column;
            }
        }
    }

    /// The box border color for a comment: the draft accent while it has
    /// uncommitted edits, matching the editor it came from, else the muted
    /// committed-comment border.
    fn comment_border(&self, pending: bool) -> Rgb {
        if pending {
            self.theme.comment_draft_fg
        } else {
            self.theme.comment_border_fg
        }
    }

    /// Returns the plain text a header row matches against in search: `#N author
    /// (kind)` for a comment, `author (kind)` for the description. The
    /// description's title also leads with a fixed "Description" word, which is
    /// not part of this text.
    fn header_search_text(&self, comment: &CommentState, id: BoxId) -> String {
        if id == BoxId::Description {
            return format!("{} ({})", comment.author.name, comment.author.kind.as_str());
        }
        comment_label(comment)
    }

    /// Builds the title shown along a comment box's top edge: its label, status
    /// badges, and a dimmed hint at the keys that act on the comment.
    fn comment_title(&self, comment: &CommentState, id: BoxId, pending: bool) -> Line<'static> {
        if id == BoxId::Description {
            return self.description_title(comment, pending);
        }
        let mut spans = vec![Span::styled(
            comment_label(comment),
            Style::default().fg(color(self.theme.comment_author_fg)),
        )];
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
        let resolve_verb = if comment.resolved {
            "unresolve"
        } else {
            "resolve"
        };
        let delete_verb = if comment.deleted {
            "undelete"
        } else {
            "delete"
        };
        // A withdrawn comment cannot take a reply, so drop the reply hint rather
        // than advertise an action the review would decline.
        let reply_hint = if comment.deleted {
            String::new()
        } else {
            format!("  {} to reply", self.hints.reply_comment)
        };
        spans.push(Span::styled(
            format!(
                "  press {} to edit{reply_hint}  {} to {resolve_verb}  {} to {delete_verb}  {} to expand/collapse",
                self.hints.edit_comment,
                self.hints.resolve_comment,
                self.hints.delete_comment,
                self.hints.toggle_comment
            ),
            Style::default().fg(color(self.theme.fold_fg)),
        ));
        Line::from(spans)
    }

    /// The title shown along the description box's top edge. Unlike a comment it
    /// cannot be resolved or deleted, so its hint offers only edit and collapse.
    fn description_title(&self, comment: &CommentState, pending: bool) -> Line<'static> {
        let mut spans = vec![
            Span::styled(
                "Description".to_string(),
                Style::default()
                    .fg(color(self.theme.review_fg))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  {} ({})",
                    comment.author.name,
                    comment.author.kind.as_str()
                ),
                Style::default().fg(color(self.theme.comment_author_fg)),
            ),
        ];
        if pending {
            spans.push(Span::styled(
                " [draft]".to_string(),
                Style::default().fg(color(self.theme.comment_draft_fg)),
            ));
        }
        spans.push(Span::styled(
            format!(
                "  press {} to edit  {} to expand/collapse",
                self.hints.edit_comment, self.hints.toggle_comment
            ),
            Style::default().fg(color(self.theme.fold_fg)),
        ));
        Line::from(spans)
    }

    /// The line shown in place of `hidden` collapsed unchanged rows, naming the
    /// enclosing `scope` when one is known.
    fn fold_marker(&self, hidden: usize, scope: Option<&str>) -> Line<'static> {
        let plural = if hidden == 1 { "" } else { "s" };
        self.fold_marker_line(&format!("{hidden} unchanged line{plural}"), scope)
    }

    /// A fold marker line: a chevron in the change-marker column, then `label` in
    /// brackets, then `scope` when one is given. The chevron in the gutter marks
    /// a fold, rather than a tinted background that would band the view.
    fn fold_marker_line(&self, label: &str, scope: Option<&str>) -> Line<'static> {
        let mut text = format!(
            "{:indent$}{FOLD_COLLAPSED} [{label}]",
            "",
            indent = GUTTER_WIDTH - 2
        );
        if let Some(scope) = scope {
            text.push_str("  ");
            text.push_str(scope);
        }
        Line::from(Span::styled(
            text,
            Style::default().fg(color(self.theme.fold_fg)),
        ))
    }

    /// The header naming a file and how it changed, tagged `[generated]` when the
    /// file was recognised as machine-generated.
    fn file_header(&self, file: &FileDiff, generated: bool) -> Line<'static> {
        let mut text = match file.status {
            FileStatus::Renamed => format!("renamed  {} -> {}", file.old_path, file.new_path),
            status => format!("{}  {}", status_label(status), file.display_path()),
        };
        if generated {
            text.push_str("  [generated]");
        }
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

    /// Render one diff line into its display rows and the background each row
    /// fills to its width, so the role tint reaches the screen edge. Each row
    /// pairs its styled line with its plain text for search. A row is the
    /// line-number gutter, the change marker, and the syntax-colored content with
    /// changed characters emphasized. A context line inside a foldable run shows
    /// `fold_mark` in the change-marker column instead of a blank. With `wrap`
    /// the content is broken to that many columns beside the gutter, each
    /// continuation indented under the code of the first row; without it the
    /// line stays a single row.
    fn content_rows(
        &self,
        line: &DiffLine,
        highlighted: Option<&HighlightedLine>,
        ranges: &[Range<usize>],
        wrap: Option<usize>,
        fold_mark: Option<char>,
    ) -> (Option<Rgb>, Vec<ContentRow>) {
        let StyledLine {
            marker,
            row_bg,
            gutter_style,
            content,
        } = self.style_line(line, highlighted, ranges, fold_mark);
        let gutter = Span::styled(
            format!(
                "{} {} {} ",
                lineno(line.old_lineno),
                lineno(line.new_lineno),
                marker,
            ),
            gutter_style,
        );
        let Some(content_width) = wrap else {
            let mut spans = vec![gutter];
            spans.extend(content);
            return (row_bg, vec![(Line::from(spans), line.text.clone())]);
        };
        // Wrap the content into the columns beside the gutter, leading the first
        // row with the gutter and each continuation with a blank gutter so the
        // wrapped code aligns under the first row.
        let blank_gutter = Span::styled(" ".repeat(GUTTER_WIDTH), gutter_style);
        let rows = wrap_line(&Line::from(content), content_width)
            .into_iter()
            .enumerate()
            .map(|(row, mut visual)| {
                let text: String = visual.spans.iter().map(|s| s.content.as_ref()).collect();
                let lead = if row == 0 {
                    gutter.clone()
                } else {
                    blank_gutter.clone()
                };
                let mut spans = vec![lead];
                spans.append(&mut visual.spans);
                (Line::from(spans), text)
            })
            .collect();
        (row_bg, rows)
    }
}

/// The role tint and syntax-colored spans of one diff line, shared by the
/// unified and side-by-side layouts, which assemble their own gutters around it.
struct StyledLine {
    marker: char,
    row_bg: Option<Rgb>,
    gutter_style: Style,
    content: Vec<Span<'static>>,
}

impl DiffView {
    /// Style one diff line into its change marker, row tint, gutter style, and
    /// syntax-colored content spans, leaving the gutter text to the caller so
    /// each layout can frame it. A foldable context line takes `fold_mark` in its
    /// marker column.
    fn style_line(
        &self,
        line: &DiffLine,
        highlighted: Option<&HighlightedLine>,
        ranges: &[Range<usize>],
        fold_mark: Option<char>,
    ) -> StyledLine {
        let (marker, row_bg, emphasis_bg) = match line.kind {
            LineKind::Context => (fold_mark.unwrap_or(' '), None, None),
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
        let gutter_fg = match row_bg {
            Some(bg) => legible_over(self.theme.gutter_fg, bg, self.theme.background),
            None => self.theme.gutter_fg,
        };
        let gutter_style = with_bg(Style::default().fg(color(gutter_fg)), row_bg);
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
            // The syntect color is picked to read on the theme background; over
            // a diff tint it can dim below legibility, so lift it back to the
            // contrast it had on the plain background.
            let fg = match bg {
                Some(bg) => legible_over(piece.style.fg, bg, self.theme.background),
                None => piece.style.fg,
            };
            content.push(Span::styled(
                piece.text,
                with_font(with_bg(Style::default().fg(color(fg)), bg), &piece.style),
            ));
        }
        // Flag trailing whitespace a change introduces, the way `git diff` warns
        // on it, since it is easy to add and hard to see.
        if line.kind == LineKind::Added {
            mark_trailing_whitespace(&mut content, &line.text, self.theme.whitespace_bg);
        }
        StyledLine {
            marker,
            row_bg,
            gutter_style,
            content,
        }
    }
}

/// One logical side-by-side row: the hunk-line indices shown in the left
/// (before) and right (after) columns, either absent where that column is blank
/// filler.
struct LinePair {
    left: Option<usize>,
    right: Option<usize>,
}

/// Pair a hunk's lines into side-by-side rows: a context line shows on both
/// sides, and a run of removed lines pairs index-wise against the run of added
/// lines that follows it, the shorter run filled with blanks.
fn pair_hunk_lines(lines: &[DiffLine]) -> Vec<LinePair> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            rows.push(LinePair {
                left: Some(i),
                right: Some(i),
            });
            i += 1;
            continue;
        }
        let mut removed = Vec::new();
        while i < lines.len() && lines[i].kind == LineKind::Removed {
            removed.push(i);
            i += 1;
        }
        let mut added = Vec::new();
        while i < lines.len() && lines[i].kind == LineKind::Added {
            added.push(i);
            i += 1;
        }
        let height = removed.len().max(added.len());
        for k in 0..height {
            rows.push(LinePair {
                left: removed.get(k).copied(),
                right: added.get(k).copied(),
            });
        }
    }
    rows
}

/// Hard-clip `line` to at most `width` display columns, counted in characters,
/// dropping any content past the limit so a side-by-side column never spills
/// past its bounds.
fn clip_line(line: &Line<'static>, width: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        if used >= width {
            break;
        }
        let count = span.content.chars().count();
        if used + count <= width {
            out.push(span.clone());
            used += count;
        } else {
            let text: String = span.content.chars().take(width - used).collect();
            out.push(Span::styled(text, span.style));
            break;
        }
    }
    Line::from(out)
}

/// A blank filler column `width` columns wide, for the side of a logical row the
/// other column outgrows.
fn blank_column(width: usize) -> Vec<Span<'static>> {
    vec![Span::styled(" ".repeat(width), Style::default())]
}

/// A run of `width` background cells in `bg`, padding a column's content out to
/// its right edge so the row tint reaches the divider.
fn pad_span(width: usize, bg: Option<Rgb>) -> Span<'static> {
    Span::styled(" ".repeat(width), with_bg(Style::default(), bg))
}

/// Join a side-by-side row's two columns into one searchable string, a space
/// between them, so a match in either column is found.
fn join_column_text(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (false, false) => format!("{left} {right}"),
        (false, true) => left.to_string(),
        (true, false) => right.to_string(),
        (true, true) => String::new(),
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

/// A run of content sharing one style and emphasis state.
struct Piece {
    text: String,
    style: wiff_diff::Style,
    emphasized: bool,
}

/// Split a line's highlighted spans at the emphasis ranges, so each run is
/// wholly inside or outside a changed range. When highlighting produced no
/// spans (an empty content line), the raw text stands in with a neutral color.
fn split_pieces(spans: &[StyledSpan], text: &str, ranges: &[Range<usize>]) -> Vec<Piece> {
    if spans.is_empty() {
        return split_span(text, 0, NEUTRAL_STYLE, ranges);
    }
    let mut out = Vec::new();
    let mut offset = 0;
    for span in spans {
        out.extend(split_span(&span.text, offset, span.style, ranges));
        offset += span.text.len();
    }
    out
}

/// Split one span, starting at byte `start` within the line, into pieces cut at
/// every emphasis-range boundary that falls inside it.
fn split_span(
    text: &str,
    start: usize,
    style: wiff_diff::Style,
    ranges: &[Range<usize>],
) -> Vec<Piece> {
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
                style,
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

/// The neutral style for content when highlighting yields no spans: the neutral
/// gray with no font emphasis.
const NEUTRAL_STYLE: wiff_diff::Style = wiff_diff::Style {
    fg: NEUTRAL_FG,
    bold: false,
    italic: false,
    underline: false,
};

/// The whole after-side text of `file`, reconstructed from its hunks. Returns
/// `None` when the capture omits any interior lines, since a partial
/// reconstruction cannot render faithfully; the trailing tail past the last
/// hunk is the natural end of the file and does not count against this.
///
/// The lines must be contiguous from line 1: the rendered view recovers each
/// row's source line by counting newlines in the joined text, which matches the
/// file's own numbering only when no line is missing before the last. A gap
/// enforces that, and each known line's own number is checked against its
/// position to catch any future drift.
fn after_side_text(file: &FileDiff) -> Option<String> {
    let mut lines = Vec::new();
    for recon in reconstitute(file, Side::After) {
        match recon {
            ReconLine::Known { text, lineno } => {
                if lineno.get() as usize != lines.len() + 1 {
                    return None;
                }
                lines.push(text);
            }
            ReconLine::Gap { count: Some(_) } => return None,
            ReconLine::Gap { count: None } => {}
        }
    }
    Some(lines.join("\n"))
}

/// A right-aligned line number, or blank space when the line is absent on this
/// side.
fn lineno(number: Option<wiff_diff::LineNo>) -> String {
    match number {
        Some(n) => format!("{:>width$}", n.get(), width = LINENO_WIDTH),
        None => " ".repeat(LINENO_WIDTH),
    }
}

/// The line count shown on a generated file's whole-file fold marker: the
/// after-side lines (context and additions) the fold hides, measured from the
/// source so it is independent of wrapping and woven-in comment rows. A pure
/// deletion has no after-side content, so its removed lines are counted instead,
/// keeping the marker from reading as hiding nothing.
fn generated_fold_line_count(file: &FileDiff) -> usize {
    let after = file
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind != LineKind::Removed)
        .count();
    if after > 0 {
        return after;
    }
    file.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == LineKind::Removed)
        .count()
}

/// The fold-column glyph for each of a hunk's `len` lines, given its foldable
/// `runs`: the first line of a run points down at the rows it collapses and the
/// rest trace the vertical line down the region; lines outside every run have
/// none.
fn fold_column(runs: &[Range<usize>], len: usize) -> Vec<Option<char>> {
    let mut marks = vec![None; len];
    for run in runs {
        marks[run.start] = Some(FOLD_EXPANDED);
        for mark in &mut marks[run.start + 1..run.end] {
            *mark = Some(FOLD_BODY);
        }
    }
    marks
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BadgeStyle {
    /// A muted badge such as `resolved`.
    Muted,
    /// A warning badge such as `shifted` or `outdated`.
    Warn,
    /// The `draft` badge for a comment with uncommitted edits.
    Draft,
}

/// The label leading a comment's title: its review-scoped number when it has
/// one, then the author and kind. A draft comment has no number yet, so it
/// leads with the author.
fn comment_label(comment: &CommentState) -> String {
    match comment.number {
        Some(number) => format!(
            "{number} {} ({})",
            comment.author.name,
            comment.author.kind.as_str()
        ),
        None => format!("{} ({})", comment.author.name, comment.author.kind.as_str()),
    }
}

/// A comment's status badges, in display order. A pending comment leads with a
/// `draft` badge. A resolved or withdrawn comment names who acted, and a comment
/// last changed by someone other than its author names that actor. A deleted
/// comment shows only that it is withdrawn, its other status being moot until it
/// is restored.
fn badges(comment: &CommentState, pending: bool) -> Vec<(String, BadgeStyle)> {
    let mut out = Vec::new();
    if pending {
        out.push(("draft".to_string(), BadgeStyle::Draft));
    }
    if comment.deleted {
        out.push((by("deleted", &comment.deleted_by), BadgeStyle::Muted));
        return out;
    }
    if comment.resolved {
        out.push((by("resolved", &comment.resolved_by), BadgeStyle::Muted));
    }
    if let Some(flag) = comment.confidence.and_then(|c| c.flag()) {
        out.push((flag.to_string(), BadgeStyle::Warn));
    }
    match comment.disposition {
        Some(Disposition::Approve) => out.push(("approve".to_string(), BadgeStyle::Muted)),
        Some(Disposition::RequestChanges) => {
            out.push(("request_changes".to_string(), BadgeStyle::Warn))
        }
        None => {}
    }
    if let Some(author) = comment.last_changed_by() {
        out.push((format!("changed by {}", author.name), BadgeStyle::Muted));
    }
    out
}

/// A status badge naming who performed an action, as `"<verb> by <name>"`, or
/// the bare verb when the actor is unknown.
fn by(verb: &str, author: &Option<Author>) -> String {
    match author {
        Some(author) => format!("{verb} by {}", author.name),
        None => verb.to_string(),
    }
}

/// A root comment paired with the replies threaded beneath it.
struct PlacedComment<'a> {
    comment: &'a CommentState,
    replies: Vec<&'a CommentState>,
}

/// A placed line comment with the range it anchors, so a fold splits around it.
struct LineComment<'a> {
    side: Side,
    start: u32,
    end: u32,
    placed: PlacedComment<'a>,
}

/// The comment inputs to one render: the comments to weave in, which of them
/// are uncommitted drafts, and how each maps onto the presented sides.
pub(crate) struct ReviewInputs<'a> {
    pub(crate) comments: &'a [CommentState],
    pub(crate) pending: &'a [Ulid],
    pub(crate) origins: &'a CommentOrigins,
    /// The review's description to render, absent when none is set.
    pub(crate) description: Option<DescriptionBox<'a>>,
    /// Whether the review has no description yet.
    pub(crate) describe_hint: bool,
}

/// The review description prepared for rendering as its leading box, reusing a
/// synthesized [`CommentState`] for the box's author, title, and body.
pub(crate) struct DescriptionBox<'a> {
    /// The synthesized comment that supplies the box's author and body.
    pub(crate) comment: &'a CommentState,
    /// Whether an uncommitted description edit is buffered.
    pub(crate) pending: bool,
}

/// How a comment's authored `(version, side)` maps onto the side it is presented
/// on. In the ordinary review the mapping is the identity; a comparison view
/// remaps because its before side shows an earlier version's content.
pub(crate) enum CommentOrigins {
    /// Present each comment on the side its target names, ignoring version.
    Literal,
    /// Present against a known after origin and a per-view before origin.
    Origins {
        /// The version and side the presented after side represents.
        after: LineOrigin,
        /// The version and side the presented before side represents.
        before: BeforeOrigins,
    },
}

/// What the presented before side represents: the latest version's before side
/// for every file (the ordinary review) or, in a comparison, a per-file origin.
pub(crate) enum BeforeOrigins {
    /// Every file's before side is `(version, Before)`.
    Baseline(u32),
    /// Each file's before origin, keyed by display path; a file absent from the
    /// map has no before side in this view.
    PerFile(HashMap<String, LineOrigin>),
}

impl CommentOrigins {
    /// The side to present `comment` on, or `None` when its content does not
    /// belong to either presented side and it should float to the file header.
    /// Only line comments map; other targets are placed by their kind.
    fn present(&self, comment: &CommentState) -> Option<Side> {
        let CommentTarget::Lines { file, side, .. } = &comment.target else {
            return None;
        };
        match self {
            CommentOrigins::Literal => Some(*side),
            CommentOrigins::Origins { after, before } => {
                if comment.version.get() == after.version && *side == after.side {
                    return Some(Side::After);
                }
                let before = match before {
                    BeforeOrigins::Baseline(version) => LineOrigin {
                        version: *version,
                        side: Side::Before,
                    },
                    BeforeOrigins::PerFile(map) => *map.get(file)?,
                };
                (comment.version.get() == before.version && *side == before.side)
                    .then_some(Side::Before)
            }
        }
    }
}

/// Where each live comment attaches within one render.
struct Placement<'a> {
    /// Review-level comments, shown under the summary row.
    review: Vec<PlacedComment<'a>>,
    /// Per-file placement, indexed by file index.
    files: Vec<FilePlacement<'a>>,
}

/// One file's placed comments.
#[derive(Default)]
struct FilePlacement<'a> {
    /// Whole-file comments, and line comments whose anchor no longer matches a
    /// rendered line, shown under the file header.
    header: Vec<PlacedComment<'a>>,
    /// Line comments, each above the line it anchors.
    lines: Vec<LineComment<'a>>,
}

impl<'a> Placement<'a> {
    /// Sort `comments` into review, whole-file, and per-line placement, threading
    /// each comment's replies beneath it. A line comment whose anchored line is
    /// no longer rendered floats to its file header. A comment naming an unknown
    /// file is skipped.
    fn new(diff: &Diff, comments: &'a [CommentState], origins: &CommentOrigins) -> Self {
        let mut files: Vec<FilePlacement<'a>> = (0..diff.files.len())
            .map(|_| FilePlacement::default())
            .collect();
        let addressable: Vec<Vec<(Side, u32)>> = diff.files.iter().map(rendered_anchors).collect();
        let index_of = |path: &str| diff.files.iter().position(|f| f.display_path() == path);
        // Each root's replies, keyed by root id, so placing a root can pick up
        // the thread beneath it. A reply is placed through its root, not here.
        let mut replies: HashMap<Ulid, Vec<&'a CommentState>> = threads(comments)
            .into_iter()
            .map(|thread| (thread.root.id, thread.replies))
            .collect();
        let mut placed = |comment: &'a CommentState| PlacedComment {
            comment,
            replies: replies.remove(&comment.id).unwrap_or_default(),
        };
        let mut review = Vec::new();
        for comment in comments {
            match &comment.target {
                CommentTarget::Review => review.push(placed(comment)),
                CommentTarget::File { file } => {
                    if let Some(i) = index_of(file) {
                        files[i].header.push(placed(comment));
                    }
                }
                CommentTarget::Lines {
                    file,
                    start_line,
                    end_line,
                    ..
                } => {
                    let Some(i) = index_of(file) else { continue };
                    let (start, end) = (start_line.get(), end_line.get());
                    match origins.present(comment) {
                        Some(side) if addressable[i].contains(&(side, start)) => {
                            files[i].lines.push(LineComment {
                                side,
                                start,
                                end,
                                placed: placed(comment),
                            });
                        }
                        _ => files[i].header.push(placed(comment)),
                    }
                }
                CommentTarget::Comment { .. } => {}
            }
        }
        Self { review, files }
    }
}

impl<'a> FilePlacement<'a> {
    /// The line comments anchored to start at the line `(side, lineno)`
    /// addresses, each with the replies threaded beneath it.
    fn at(&self, side: Side, lineno: u32) -> impl Iterator<Item = &LineComment<'a>> + '_ {
        self.lines
            .iter()
            .filter(move |lc| lc.side == side && lc.start == lineno)
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

/// Apply a highlighted span's font emphasis (bold, italic, underline) to a
/// ratatui style.
fn with_font(mut style: Style, font: &wiff_diff::Style) -> Style {
    if font.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if font.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if font.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
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

    use crate::theme::Theme;

    /// The syntax theme every rendering test pins to, held apart from the
    /// shipped default so changing that default never churns snapshots that
    /// assert exact colors.
    pub(crate) const TEST_THEME: &str = "base16-ocean.dark";

    /// The palette every rendering test paints with, pinned to [`TEST_THEME`].
    pub(crate) fn theme() -> Theme {
        Theme::named(TEST_THEME).expect("bundled test theme")
    }

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
        let mut flags = String::new();
        if modifier.contains(Modifier::BOLD) {
            flags.push('b');
        }
        if modifier.contains(Modifier::ITALIC) {
            flags.push('i');
        }
        if modifier.contains(Modifier::UNDERLINED) {
            flags.push('u');
        }
        if modifier.contains(Modifier::CROSSED_OUT) {
            flags.push('s');
        }
        if flags.is_empty() {
            "-".to_string()
        } else {
            flags
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;
    use time::OffsetDateTime;
    use ulid::Ulid;
    use wiff_core::record::{
        Author, AuthorKind, CommentTarget, Confidence, Disposition, Seq, VersionNumber,
    };
    use wiff_core::review::CommentState;
    use wiff_diff::{Diff, FileStatus, LineKind, Side};

    use super::testutil::{dump, file, ln, theme};
    use super::{BadgeStyle, BoxId, CommentRegion, DiffView, ViewLayout, badges};

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
            version: VersionNumber(0),
            anchor: None,
            body: body.to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: Author {
                name: author.0.to_string(),
                kind: author.1,
            },
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            disposition: None,
            confidence: None,
            origin: None,
            synced: None,
            // This hand-built state feeds rendering directly, so the number is
            // whatever the snapshot asserts, not a fold's output. Fixtures pass
            // ids in create order, so reusing the id as the number reads
            // naturally.
            number: Some(wiff_core::record::CommentNumber(id as u32)),
            created_seq: Seq(0),
            updated_seq: Seq(0),
        }
    }

    #[test]
    fn a_request_changes_verdict_badges_as_a_warning() {
        let mut comment = comment(1, ("wez", AuthorKind::Human), CommentTarget::Review, "no");
        comment.disposition = Some(Disposition::RequestChanges);
        wince::assert_eq!(
            badges(&comment, false),
            vec![("request_changes".to_string(), BadgeStyle::Warn)]
        );
    }

    #[test]
    fn an_approve_verdict_follows_the_draft_and_resolved_badges() {
        let mut comment = comment(1, ("wez", AuthorKind::Human), CommentTarget::Review, "ok");
        comment.resolved = true;
        comment.disposition = Some(Disposition::Approve);
        wince::assert_eq!(
            badges(&comment, true),
            vec![
                ("draft".to_string(), BadgeStyle::Draft),
                ("resolved".to_string(), BadgeStyle::Muted),
                ("approve".to_string(), BadgeStyle::Muted),
            ]
        );
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
            let id = match region.id {
                BoxId::Comment(ulid) => ulid.0.to_string(),
                BoxId::Description => "description".to_string(),
            };
            out.push_str(&format!(
                "{}: header {} body {}..{} collapsed={}\n",
                id, region.header, region.body.start, region.body.end, region.collapsed_default,
            ));
        }
        out
    }

    #[test]
    fn side_by_side_pairs_changes_across_columns_and_fills_the_short_side() {
        // A context line shows on both sides; a removed line pairs against the
        // first added line of the following run; the surplus added line pairs
        // against a blank left column. Each column has its own line-number
        // gutter and clips to its half, split by the divider rule.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Context, "let x = 1;", 1),
                    (LineKind::Removed, "let y = 2;", 2),
                    (LineKind::Added, "let y = 3;", 2),
                    (LineKind::Added, "let z = 4;", 3),
                ],
            )],
        };
        let view = DiffView::new(theme()).unwrap();
        let layout = ViewLayout {
            width: 44,
            wrap_content: false,
            mode: super::LayoutMode::SideBySide,
        };

        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render_review(&diff, &[], &[], layout).lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,4 +1,4 @@\n",
            "<#7d828c|-|->   1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;<-|-|->    <#7d828c|-|->│<#7d828c|-|->   1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;<-|-|->     \n",
            "<#91959d|#463943|->   2 - <#bf9fb9|#463943|->let<#c0c5ce|#463943|-> y <#c0c5ce|#463943|->=<#c0c5ce|#463943|-> <#e3b7a9|#66444e|->2<#c0c5ce|#66444e|->;<-|#463943|->    <#7d828c|-|->│<#9ea1a9|#414a4a|->   2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#f3e1db|#5b695b|->3<#e3e5e9|#5b695b|->;<-|#414a4a|->     \n",
            "<-|-|->                     <#7d828c|-|->│<#9ea1a9|#414a4a|->   3 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> z <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->4<#c0c5ce|#414a4a|->;<-|#414a4a|->     \n",
        );
    }

    #[test]
    fn only_after_drops_removed_lines_and_keeps_the_after_column() {
        // The removed line contributes no row; the context and added lines show
        // in one column with the ordinary two-number gutter, so the column reads
        // as the resulting file.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Context, "let x = 1;", 1),
                    (LineKind::Removed, "let y = 2;", 2),
                    (LineKind::Added, "let y = 3;", 2),
                    (LineKind::Added, "let z = 4;", 3),
                ],
            )],
        };
        let view = DiffView::new(theme()).unwrap();
        let layout = ViewLayout {
            width: 40,
            wrap_content: false,
            mode: super::LayoutMode::OnlyAfter,
        };

        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render_review(&diff, &[], &[], layout).lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,4 +1,4 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#f3e1db|#5b695b|->3<#e3e5e9|#5b695b|->;\n",
            "<#9ea1a9|#414a4a|->        3 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> z <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->4<#c0c5ce|#414a4a|->;\n",
        );
    }

    #[test]
    fn rendered_mode_shows_markdown_formatted_with_source_line_gutter() {
        // An added markdown file renders as formatted text in one column: the
        // heading loses its `#` marker and the gutter numbers each source line
        // once, so the reviewer reads the resulting document.
        let diff = Diff {
            files: vec![file(
                "README.md",
                FileStatus::Added,
                &[
                    (LineKind::Added, "# Title", 1),
                    (LineKind::Added, "body text", 2),
                ],
            )],
        };
        let view = DiffView::new(theme()).unwrap();
        let layout = ViewLayout {
            width: 40,
            wrap_content: false,
            mode: super::LayoutMode::Rendered,
        };

        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render_review(&diff, &[], &[], layout).lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>added  README.md\n",
            "<#7d828c|-|->        1   <#ebcb8b|-|bu>Title\n",
            "<#7d828c|-|->            \n",
            "<#7d828c|-|->        2   <#c0c5ce|-|->body text\n",
        );
    }

    #[test]
    fn a_rendered_comment_opens_above_its_source_line() {
        // A comment anchored to the after-side line 2 opens above that line's
        // first rendered row, so annotating the formatted document anchors to
        // the right source line.
        let diff = Diff {
            files: vec![file(
                "README.md",
                FileStatus::Added,
                &[
                    (LineKind::Added, "# Title", 1),
                    (LineKind::Added, "body text", 2),
                ],
            )],
        };
        let comments = vec![comment(
            1,
            ("wez", AuthorKind::Human),
            on_lines("README.md", 2, 2),
            "reword this",
        )];
        let view = DiffView::new(theme()).unwrap();
        let layout = ViewLayout {
            width: 40,
            wrap_content: false,
            mode: super::LayoutMode::Rendered,
        };

        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render_review(&diff, &comments, &[], layout).lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>added  README.md\n",
            "<#7d828c|-|->        1   <#ebcb8b|-|bu>Title\n",
            "<#7d828c|-|->            \n",
            "<#8fa1b3|-|->#1 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->reword this\n",
            "\n",
            "<#7d828c|-|->        2   <#c0c5ce|-|->body text\n",
        );
    }

    #[test]
    fn a_rendered_comment_with_no_rendered_row_floats_to_the_file_header() {
        // The comment anchors to the removed (before-side) line 2, which the
        // rendered after side never shows. Rather than vanish, it floats to the
        // file header, above the rendered content.
        let diff = Diff {
            files: vec![file(
                "README.md",
                FileStatus::Modified,
                &[
                    (LineKind::Context, "# Title", 1),
                    (LineKind::Removed, "old body", 2),
                    (LineKind::Added, "new body", 2),
                ],
            )],
        };
        let comments = vec![comment(
            1,
            ("wez", AuthorKind::Human),
            CommentTarget::Lines {
                file: "README.md".to_string(),
                side: Side::Before,
                start_line: ln(2),
                end_line: ln(2),
            },
            "this line went away",
        )];
        let view = DiffView::new(theme()).unwrap();
        let layout = ViewLayout {
            width: 40,
            wrap_content: false,
            mode: super::LayoutMode::Rendered,
        };

        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render_review(&diff, &comments, &[], layout).lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  README.md\n",
            "<#8fa1b3|-|->#1 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->this line went away\n",
            "\n",
            "<#7d828c|-|->        1   <#ebcb8b|-|bu>Title\n",
            "<#7d828c|-|->            \n",
            "<#7d828c|-|->        2   <#c0c5ce|-|->new body\n",
        );
    }

    #[test]
    fn rendered_mode_falls_back_to_the_after_column_without_a_renderer() {
        // A source file has no whole-file renderer, so the rendered layout shows
        // its after side as source, dropping the removed line like only-after.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Context, "let x = 1;", 1),
                    (LineKind::Removed, "let y = 2;", 2),
                    (LineKind::Added, "let y = 3;", 2),
                ],
            )],
        };
        let view = DiffView::new(theme()).unwrap();
        let layout = ViewLayout {
            width: 40,
            wrap_content: false,
            mode: super::LayoutMode::Rendered,
        };

        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render_review(&diff, &[], &[], layout).lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,3 +1,3 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#f3e1db|#5b695b|->3<#e3e5e9|#5b695b|->;\n",
        );
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
        let view = DiffView::new(theme()).unwrap();

        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render(&diff).lines),
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
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
        let view = DiffView::new(theme()).unwrap();

        // "fred"/"pete" (bytes 12..16) get the stronger emphasis background; the
        // unchanged run keeps the plain role tint. The .txt content has no
        // syntax, so it is one neutral color.
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render(&diff).lines),
            "<#c0c5ce|-|b>modified  greeting.txt\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#91959d|#463943|->   1      - <#c0c5ce|#463943|->hello there <#c0c5ce|#66444e|->fred\n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->hello there <#e3e5e9|#5b695b|->pete\n",
        );
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
        let view = DiffView::new(theme()).unwrap();
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&view.render(&diff).lines),
            "<#c0c5ce|-|b>modified  notes.txt\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#91959d|#463943|->   1      - <#c0c5ce|#463943|->old \n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->new<#c0c5ce|#7c4b55|->  \n",
        );
    }

    #[test]
    fn a_fold_marker_names_the_enclosing_definition_of_the_content_below_it() {
        // A rust change buried below its function: the leading context folds and
        // its marker names the enclosing fn, found by scanning up past the
        // hidden lines.
        let mut lines: Vec<(LineKind, String, u32)> =
            vec![(LineKind::Context, "fn draw() {".to_string(), 1)];
        for n in 2..=10 {
            lines.push((LineKind::Context, format!("    let v{n} = {n};"), n));
        }
        lines.push((LineKind::Added, "    let w = 11;".to_string(), 11));
        let borrowed: Vec<(LineKind, &str, u32)> =
            lines.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect();
        let diff = Diff {
            files: vec![file("src/lib.rs", FileStatus::Modified, &borrowed)],
        };
        let doc = DiffView::new(theme()).unwrap().render(&diff);
        let markers: Vec<Line<'static>> = doc.folds.iter().map(|f| f.marker.clone()).collect();
        wince::snapshot_str!(
            dump(&markers),
            "<#767b84|-|->          ▸ [7 unchanged lines]  fn draw() {\n"
        );
    }

    /// One row per fold: its hidden range, whether it hides a whole file, and
    /// whether it starts collapsed.
    fn folds_repr(folds: &[super::Fold]) -> String {
        let mut out = String::new();
        for fold in folds {
            out.push_str(&format!(
                "{}..{} whole_file={} collapsed_default={}\n",
                fold.start, fold.end, fold.whole_file, fold.collapsed_default
            ));
        }
        out
    }

    #[test]
    fn a_generated_file_badges_its_header_and_folds_its_body_by_default() {
        // A lock file is generated by name, so its header wears the badge and its
        // whole body folds to one marker, collapsed by default.
        let diff = Diff {
            files: vec![file(
                "yarn.lock",
                FileStatus::Added,
                &[(LineKind::Added, "alpha", 1), (LineKind::Added, "beta", 2)],
            )],
        };
        let doc = DiffView::new(theme()).unwrap().render(&diff);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&doc.lines),
            "<#c0c5ce|-|b>added  yarn.lock  [generated]\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->alpha\n",
            "<#9ea1a9|#414a4a|->        2 + <#c0c5ce|#414a4a|->beta\n",
        );
        wince::snapshot_str!(
            folds_repr(&doc.folds),
            "1..4 whole_file=true collapsed_default=true\n"
        );
        wince::snapshot_str!(
            dump(
                &doc.folds
                    .iter()
                    .map(|f| f.marker.clone())
                    .collect::<Vec<_>>()
            ),
            "<#767b84|-|->          ▸ [2 lines]\n"
        );
    }

    #[test]
    fn a_deleted_generated_file_counts_its_removed_lines() {
        // A deleted lock file has no after-side content, so the fold marker
        // counts the removed lines it hides rather than reading as [0 lines].
        let diff = Diff {
            files: vec![file(
                "yarn.lock",
                FileStatus::Deleted,
                &[
                    (LineKind::Removed, "alpha", 1),
                    (LineKind::Removed, "beta", 2),
                    (LineKind::Removed, "gamma", 3),
                ],
            )],
        };
        let doc = DiffView::new(theme()).unwrap().render(&diff);
        wince::snapshot_str!(
            dump(&[doc.lines[0].clone()]),
            "<#c0c5ce|-|b>deleted  yarn.lock  [generated]\n"
        );
        wince::snapshot_str!(
            folds_repr(&doc.folds),
            "1..5 whole_file=true collapsed_default=true\n"
        );
        wince::snapshot_str!(
            dump(
                &doc.folds
                    .iter()
                    .map(|f| f.marker.clone())
                    .collect::<Vec<_>>()
            ),
            "<#767b84|-|->          ▸ [3 lines]\n"
        );
    }

    #[test]
    fn a_generation_marker_near_the_top_folds_the_file() {
        // No name match, but a generator banner in the head marks the file
        // generated; the badge and whole-file fold follow.
        let banner = format!("// @{} by build.rs", "generated");
        let diff = Diff {
            files: vec![file(
                "src/tables.rs",
                FileStatus::Added,
                &[
                    (LineKind::Added, banner.as_str(), 1),
                    (LineKind::Added, "pub const N: u8 = 1;", 2),
                ],
            )],
        };
        let doc = DiffView::new(theme()).unwrap().render(&diff);
        wince::snapshot_str!(
            dump(&[doc.lines[0].clone()]),
            "<#c0c5ce|-|b>added  src/tables.rs  [generated]\n"
        );
        wince::snapshot_str!(
            folds_repr(&doc.folds),
            "1..4 whole_file=true collapsed_default=true\n"
        );
    }

    #[test]
    fn a_line_comment_keeps_a_generated_file_open_by_default() {
        // A comment on one of a generated file's lines lives inside the folded
        // body, so the fold starts open to keep the discussion visible.
        let diff = Diff {
            files: vec![file(
                "yarn.lock",
                FileStatus::Added,
                &[(LineKind::Added, "alpha", 1)],
            )],
        };
        let comments = vec![comment(
            1,
            ("wez", AuthorKind::Human),
            on_lines("yarn.lock", 1, 1),
            "regenerate this",
        )];
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[],
            ViewLayout::default(),
        );
        wince::snapshot_str!(
            folds_repr(&doc.folds),
            "2..7 whole_file=true collapsed_default=false\n"
        );
    }

    #[test]
    fn a_whole_file_comment_stays_above_a_collapsed_generated_file() {
        // A whole-file comment renders above the fold, so it stays visible while
        // the body collapses by default.
        let diff = Diff {
            files: vec![file(
                "yarn.lock",
                FileStatus::Added,
                &[(LineKind::Added, "alpha", 1)],
            )],
        };
        let comments = vec![comment(
            1,
            ("wez", AuthorKind::Human),
            CommentTarget::File {
                file: "yarn.lock".to_string(),
            },
            "regenerate this",
        )];
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[],
            ViewLayout::default(),
        );
        wince::snapshot_str!(
            folds_repr(&doc.folds),
            "5..7 whole_file=true collapsed_default=true\n"
        );
    }

    #[test]
    fn foldable_runs_keep_context_around_changes_and_fold_the_rest() {
        use LineKind::{Added as A, Context as C};
        // Leading, interior, and trailing runs of context, keeping one line on
        // each side of the two changes. The leading and trailing runs are long
        // enough to fold; the interior run of four context lines is too short.
        let kinds = [
            C, C, C, C, C, C, C, A, C, C, C, C, C, C, A, C, C, C, C, C, C, C,
        ];
        let none = vec![false; kinds.len()];
        wince::assert_eq!(super::foldable_runs(&kinds, 1, &none), vec![0..6, 16..22]);
        // With enough context to reach across every gap, nothing folds.
        wince::assert_eq!(
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
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[],
            ViewLayout::default(),
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&doc.lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#8fa1b3|-|->#1 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->why 2?\n",
            "\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
        // The header row precedes the single body row, which is the collapsible
        // range; an unresolved comment starts expanded.
        wince::snapshot_str!(
            regions(&doc.comments),
            "1: header 4 body 5..6 collapsed=false\n"
        );

        // The cached render, reusing a prior highlight of the same diff, must
        // produce the identical lines; caching is a speed-up, not a change.
        let view = DiffView::new(theme()).unwrap();
        let highlights: Vec<_> = view
            .recolor(&view.parse(&diff))
            .into_iter()
            .map(Some)
            .collect();
        let cached =
            view.render_review_cached(&diff, &comments, &[], &highlights, ViewLayout::default());
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&cached.lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#8fa1b3|-|->#1 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->why 2?\n",
            "\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
    }

    #[test]
    fn a_reply_renders_as_its_own_box_right_after_the_comment_it_answers() {
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
        let reply = comment(
            2,
            ("opus", AuthorKind::Agent),
            CommentTarget::Comment { id: Ulid(1) },
            "because two",
        );
        let comments = vec![
            comment(
                1,
                ("wez", AuthorKind::Human),
                on_lines("src/lib.rs", 2, 2),
                "why 2?",
            ),
            reply,
        ];
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[],
            ViewLayout::default(),
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&doc.lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#8fa1b3|-|->#1 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->why 2?\n",
            "\n",
            "<#8fa1b3|-|->#2 opus (agent)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->because two\n",
            "\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
        wince::snapshot_str!(
            regions(&doc.comments),
            "1: header 4 body 5..6 collapsed=false\n",
            "2: header 7 body 8..9 collapsed=false\n",
        );
    }

    #[test]
    fn a_multi_line_range_traces_the_anchor_rail_down_the_gutter() {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Added, "let a = 1;", 1),
                    (LineKind::Added, "let b = 2;", 2),
                    (LineKind::Added, "let c = 3;", 3),
                ],
            )],
        };
        let comments = vec![comment(
            1,
            ("wez", AuthorKind::Human),
            on_lines("src/lib.rs", 1, 3),
            "extract a helper",
        )];
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[],
            ViewLayout::default(),
        );
        // The rail runs down the last gutter cell of the covered lines: the body
        // glyph on the first two and the closing corner on the last; every other
        // row leaves the cell empty. The app layer paints it in at draw time.
        let rail: String = doc
            .rails
            .iter()
            .map(|cells| cells.first().map_or('.', |c| c.glyph))
            .collect();
        wince::assert_eq!(rail, "......\u{2502}\u{2502}\u{2514}");
    }

    #[test]
    fn the_anchor_rail_runs_unbroken_through_an_interleaved_removed_line() {
        // A removed line sits between the after-side lines the comment covers.
        // The rail traces the body glyph through it too, so the covered span
        // reads as one unbroken stroke rather than breaking at the deletion.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Added, "let a = 1;", 1),
                    (LineKind::Removed, "let gone = 0;", 5),
                    (LineKind::Added, "let b = 2;", 2),
                    (LineKind::Added, "let c = 3;", 3),
                ],
            )],
        };
        let comments = vec![comment(
            1,
            ("wez", AuthorKind::Human),
            on_lines("src/lib.rs", 1, 3),
            "extract a helper",
        )];
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[],
            ViewLayout::default(),
        );
        let rail: String = doc
            .rails
            .iter()
            .map(|cells| cells.first().map_or('.', |c| c.glyph))
            .collect();
        wince::assert_eq!(rail, "......\u{2502}\u{2502}\u{2502}\u{2514}");
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
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[Ulid(1)],
            ViewLayout::default(),
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&doc.lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#8fa1b3|-|->#1 wez (human)<#a3be8c|-|-> [draft]<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->why 2?\n",
            "\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
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
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &[resolved],
            &[],
            ViewLayout::default(),
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&doc.lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#8fa1b3|-|->#7 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->done\n",
            "\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
        wince::snapshot_str!(
            regions(&doc.comments),
            "7: header 3 body 4..5 collapsed=true\n"
        );
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
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[],
            ViewLayout::default(),
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&doc.lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#8fa1b3|-|->#3 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->looks good overall\n",
            "\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
    }

    #[test]
    fn a_line_comment_inside_a_long_run_splits_the_fold_around_it() {
        // A comment on a context line far from the change keeps that line and
        // its surrounding context, leaving a fold both above and below it rather
        // than hiding the commented line. Both runs are long enough to fold on
        // their own.
        let mut lines: Vec<(LineKind, String, u32)> = (1..=29)
            .map(|n| (LineKind::Context, format!("ctx{n:02}"), n))
            .collect();
        lines.push((LineKind::Added, "change!".to_string(), 30));
        let borrowed: Vec<(LineKind, &str, u32)> =
            lines.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect();
        let diff = Diff {
            files: vec![file("notes.txt", FileStatus::Modified, &borrowed)],
        };
        let comments = vec![comment(
            9,
            ("wez", AuthorKind::Human),
            on_lines("notes.txt", 10, 10),
            "here",
        )];
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &comments,
            &[],
            ViewLayout::default(),
        );
        let markers: Vec<Line<'static>> = doc.folds.iter().map(|f| f.marker.clone()).collect();
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&markers),
            "<#767b84|-|->          ▸ [6 unchanged lines]  ctx06\n",
            "<#767b84|-|->          ▸ [13 unchanged lines]  ctx26\n",
        );
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
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &[outdated],
            &[],
            ViewLayout::default(),
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&doc.lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#8fa1b3|-|->#5 dev (human)<#d08770|-|-> [outdated]<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->stale\n",
            "\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
    }

    #[test]
    fn a_comment_changed_by_another_actor_names_that_actor_in_a_badge() {
        // An agent that reanchored or edited a human's comment is named in a
        // muted badge, so the reviewer sees their comment was touched and by
        // whom.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Added, "let y = 2;", 1)],
            )],
        };
        let mut changed = comment(
            8,
            ("wez", AuthorKind::Human),
            on_lines("src/lib.rs", 1, 1),
            "why 2?",
        );
        changed.updated_by = Author {
            name: "opus".to_string(),
            kind: AuthorKind::Agent,
        };
        changed.updated_seq = Seq(3);
        let doc = DiffView::new(theme()).unwrap().render_review(
            &diff,
            &[changed],
            &[],
            ViewLayout::default(),
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&doc.lines),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#8fa1b3|-|->#8 wez (human)<#767b84|-|-> [changed by opus]<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->why 2?\n",
            "\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
    }
}
