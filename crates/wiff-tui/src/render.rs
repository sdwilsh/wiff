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

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ulid::Ulid;
use wiff_core::LineOrigin;
use wiff_core::record::{Author, CommentTarget, Confidence};
use wiff_core::review::CommentState;
use wiff_diff::{
    Diff, DiffLine, FileDiff, FileStatus, HighlightError, HighlightedLine, Highlighter, LineKind,
    LineNo, LiveHighlighter, ParsedSide, Parser, Rgb, Section, SectionMatchers, Side, StyledSpan,
    intraline,
};

use crate::action::Action;
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

/// The shortest run of unchanged lines worth collapsing. Hiding a single line
/// behind a one-line marker saves nothing, so only runs of two or more fold.
const MIN_FOLD: usize = 2;

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

/// The anchor rail a content row draws in its last gutter cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailCell {
    /// The rail glyph: the body tracing the range down, or the corner closing it
    /// on the range's final line.
    pub glyph: char,
    /// The comment-box border color, matching the rail to its box.
    pub color: Rgb,
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
    /// The anchor rail each row draws in the last gutter cell, parallel to
    /// `lines`; `None` for a row no comment range covers. Applied when comments
    /// are shown and dropped with them, since the rail is annotation chrome.
    pub rails: Vec<Option<RailCell>>,
    /// The foldable runs of unchanged rows, in row order, non-overlapping.
    pub folds: Vec<Fold>,
    /// The collapsible comment bodies, in row order.
    pub comments: Vec<CommentRegion>,
    /// The display path of each file, indexed by [`Row::file`].
    pub files: Vec<String>,
}

/// A rendered comment box: its top-edge header row and the body rows that
/// collapse behind it, closed off by a bottom-edge row that stays visible even
/// when the body is collapsed away.
pub struct CommentRegion {
    /// The annotation's stable identity.
    pub id: Ulid,
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
    /// A content line, anchorable to `(side, lineno)` on the diff. A row
    /// addresses a Before-side line in its left column and an After-side line in
    /// its right column. Unified fills only one column per row; side-by-side may
    /// fill both, with the other column blank filler where the sides differ in
    /// length.
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
        /// The annotation the header belongs to.
        id: Ulid,
    },
    /// One line of a comment's body, inside the box.
    CommentBody {
        /// The annotation the body belongs to.
        id: Ulid,
    },
    /// A comment box's bottom edge, closing the box below its body.
    CommentBottom {
        /// The annotation the box belongs to.
        id: Ulid,
    },
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
        self.rails.push(None);
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
    hints: KeyHints,
}

/// The key labels the review view names in its hints, resolved from the active
/// keymap so each hint shows the reviewer's own binding.
pub struct KeyHints {
    /// The key that drafts a new comment.
    pub add_comment: String,
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
            hints: KeyHints::default(),
        })
    }

    /// Recolor the renderer to `theme`, swapping only the syntax highlighter's
    /// color mapping and keeping the loaded syntaxes, display context, section
    /// matchers, and key hints already configured. Reusing a [`parse`](Self::parse)
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

    /// Render every file of `diff` into one scrollable [`Document`], with no
    /// review overlay.
    pub fn render(&self, diff: &Diff) -> Document {
        self.build(
            diff,
            ReviewInputs {
                comments: &[],
                pending: &[],
                origins: &CommentOrigins::Literal,
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
        comments: &[CommentState],
        pending: &[Ulid],
        highlights: &[Option<FileHighlights>],
        layout: ViewLayout,
        origins: &CommentOrigins,
    ) -> Document {
        self.build(
            diff,
            ReviewInputs {
                comments,
                pending,
                origins,
            },
            true,
            Some(highlights),
            layout,
        )
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
        } = inputs;
        let mut doc = Document {
            lines: Vec::new(),
            rails: Vec::new(),
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
        };
        let placement = Placement::new(diff, comments, origins);
        if review_row {
            doc.push(
                NO_FILE,
                RowKind::ReviewSummary,
                Some(self.theme.status_bg),
                String::new(),
                self.review_summary(),
            );
            for comment in &placement.review {
                self.push_comment(
                    &mut doc,
                    NO_FILE,
                    comment,
                    pending.contains(&comment.id),
                    layout.width,
                    false,
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
        doc.push(
            index,
            RowKind::FileHeader,
            None,
            file.display_path().to_string(),
            self.file_header(file),
        );
        for comment in &placement.header {
            self.push_comment(
                doc,
                index,
                comment,
                pending.contains(&comment.id),
                width,
                false,
            );
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
            // one past a line's last row, not one past its first; track both.
            let mut line_row = Vec::with_capacity(hunk.lines.len());
            let mut line_end = Vec::with_capacity(hunk.lines.len());
            let content_wrap = if layout.wrap_content && width > GUTTER_WIDTH {
                Some(width - GUTTER_WIDTH)
            } else {
                None
            };
            // Work out the foldable runs up front so each context line inside
            // one can show a fold-column glyph, marking the region as
            // collapsible even while it is expanded.
            let kinds: Vec<LineKind> = hunk.lines.iter().map(|line| line.kind).collect();
            let anchored: Vec<bool> = hunk
                .lines
                .iter()
                .map(|line| line_anchor(line).is_some_and(|(s, n)| placement.covers(s, n)))
                .collect();
            let runs = foldable_runs(&kinds, self.display_context, &anchored);
            let fold_marks = fold_column(&runs, hunk.lines.len());
            for (line_index, (line, ranges)) in hunk.lines.iter().zip(&emphasis).enumerate() {
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
                            pending.contains(&comment.id),
                            width,
                            true,
                        );
                    }
                }
                line_row.push(doc.rows.len());
                // Remember the first display row of each content line, keyed by
                // side and number, so the rail post-pass can walk a comment's
                // covered span in document order.
                if let Some(n) = lineno {
                    first_row.entry((side, n.get())).or_insert(doc.rows.len());
                }
                let (fill, rows) = self.content_rows(
                    line,
                    highlighted,
                    ranges,
                    content_wrap,
                    fold_marks[line_index],
                );
                for (rendered, text) in rows {
                    doc.push(
                        index,
                        RowKind::content(side, lineno),
                        fill,
                        text,
                        rendered,
                    );
                }
                line_end.push(doc.rows.len());
            }
            let section = self.sections.for_path(file.display_path());
            for run in runs {
                let scope = enclosing_scope(&hunk.lines, run.end, &section);
                doc.folds.push(Fold {
                    start: line_row[run.start],
                    end: line_end[run.end - 1],
                    marker: self.fold_marker(run.end - run.start, scope),
                    fill: None,
                });
            }
        }
        self.trace_rails(doc, placement, pending, &first_row);
    }

    /// Draw each line comment's anchor rail into `doc.rails`. A comment's rail
    /// runs from the first display row of its start line down to the first
    /// display row of its end line, closing with the corner there; every row
    /// between traces the body glyph, including the opposite-side rows a unified
    /// diff interleaves, so the rail reads as one unbroken stroke down the
    /// gutter. Where spans overlap the one column, the box reaching deepest wins
    /// the row: its color and, at its last line, the closing corner.
    fn trace_rails(
        &self,
        doc: &mut Document,
        placement: &FilePlacement,
        pending: &[Ulid],
        first_row: &HashMap<(Side, u32), usize>,
    ) {
        // The deepest closing row recorded for each railed row, so a shallower
        // comment does not overwrite a deeper one sharing the column.
        let mut reach: HashMap<usize, usize> = HashMap::new();
        for lc in &placement.lines {
            let (Some(&start_row), Some(&end_row)) = (
                first_row.get(&(lc.side, lc.start)),
                first_row.get(&(lc.side, lc.end)),
            ) else {
                continue;
            };
            let color = self.comment_border(pending.contains(&lc.comment.id));
            for row in start_row..=end_row {
                if !matches!(doc.rows[row].kind, RowKind::Content { .. }) {
                    continue;
                }
                if reach.get(&row).is_some_and(|&deepest| deepest >= end_row) {
                    continue;
                }
                reach.insert(row, end_row);
                let glyph = if row == end_row { RAIL_END } else { RAIL_BODY };
                doc.rails[row] = Some(RailCell { glyph, color });
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
                format!(
                    " [press {} here to draft the review comment]",
                    self.hints.add_comment
                ),
                Style::default()
                    .fg(color(legible_over(
                        self.theme.fold_fg,
                        self.theme.status_bg,
                        self.theme.background,
                    )))
                    .bg(bg),
            ),
        ])
    }

    /// Append `comment` as a box: a top-edge header row, its body rows, and a
    /// bottom-edge row, all sharing the box border color. Each of the body's own
    /// lines is wrapped to fit the box interior at `width`, so a long line an
    /// agent writes on one row spreads across several rows the reviewer can read
    /// without scrolling sideways. The header and bottom rows stay visible when
    /// the body collapses, so a folded comment still reads as a closed box.
    /// Records the collapsible body range so a resolved comment starts collapsed.
    fn push_comment(
        &self,
        doc: &mut Document,
        file: usize,
        comment: &CommentState,
        pending: bool,
        width: usize,
        rail: bool,
    ) {
        let border = self.comment_border(pending);
        let header = doc.rows.len();
        doc.push(
            file,
            RowKind::CommentHeader { id: comment.id },
            Some(border),
            comment.author.name.clone(),
            self.comment_title(comment, pending),
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
            doc.push(
                file,
                RowKind::CommentBody { id: comment.id },
                Some(border),
                plain,
                line,
            );
        }
        let body_end = doc.rows.len();
        doc.push(
            file,
            RowKind::CommentBottom { id: comment.id },
            Some(border),
            String::new(),
            Line::default(),
        );
        doc.comments.push(CommentRegion {
            id: comment.id,
            header,
            body: body_start..body_end,
            collapsed_default: comment.resolved || comment.deleted,
            anchor_rail: rail,
        });
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

    /// The title shown along a comment box's top edge: the author and kind, the
    /// status badges, and a dimmed hint at the keys that edit, resolve, delete,
    /// and expand or collapse the comment.
    fn comment_title(&self, comment: &CommentState, pending: bool) -> Line<'static> {
        let mut spans = vec![Span::styled(
            format!("{} ({})", comment.author.name, comment.author.kind.as_str()),
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
        spans.push(Span::styled(
            format!(
                "  press {} to edit  {} to {resolve_verb}  {} to {delete_verb}  {} to expand/collapse",
                self.hints.edit_comment,
                self.hints.resolve_comment,
                self.hints.delete_comment,
                self.hints.toggle_comment
            ),
            Style::default().fg(color(self.theme.fold_fg)),
        ));
        Line::from(spans)
    }

    /// The line shown in place of `hidden` collapsed rows, indented to align
    /// under the code column and naming the enclosing `scope` when one is known.
    fn fold_marker(&self, hidden: usize, scope: Option<&str>) -> Line<'static> {
        let plural = if hidden == 1 { "" } else { "s" };
        // A chevron in the change-marker column marks the collapsed rows, so a
        // fold reads as a fold from the gutter alone rather than from a tinted
        // background that would band the view.
        let mut text = format!(
            "{:indent$}{FOLD_COLLAPSED} [{hidden} unchanged line{plural}]",
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

/// A right-aligned line number, or blank space when the line is absent on this
/// side.
fn lineno(number: Option<wiff_diff::LineNo>) -> String {
    match number {
        Some(n) => format!("{:>width$}", n.get(), width = LINENO_WIDTH),
        None => " ".repeat(LINENO_WIDTH),
    }
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
enum BadgeStyle {
    /// A muted badge such as `resolved`.
    Muted,
    /// A warning badge such as `shifted` or `outdated`.
    Warn,
    /// The `draft` badge for a comment with uncommitted edits.
    Draft,
}

/// A comment's status badges, in display order. A pending comment leads with a
/// `draft` badge so uncommitted work stands out. A resolved or withdrawn comment
/// names who acted, so the reviewer sees at a glance who cleared it. A deleted
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
    match comment.confidence {
        Some(Confidence::Approximate) => out.push(("shifted".to_string(), BadgeStyle::Warn)),
        Some(Confidence::Outdated) => out.push(("outdated".to_string(), BadgeStyle::Warn)),
        Some(Confidence::Exact) | None => {}
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

/// A live line comment with the range it anchors, so a fold splits around it.
struct LineComment<'a> {
    side: Side,
    start: u32,
    end: u32,
    comment: &'a CommentState,
}

/// The comment inputs to one render: the comments to weave in, which of them
/// are uncommitted drafts, and how each maps onto the presented sides.
struct ReviewInputs<'a> {
    comments: &'a [CommentState],
    pending: &'a [Ulid],
    origins: &'a CommentOrigins,
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
                if comment.version == after.version && *side == after.side {
                    return Some(Side::After);
                }
                let before = match before {
                    BeforeOrigins::Baseline(version) => LineOrigin {
                        version: *version,
                        side: Side::Before,
                    },
                    BeforeOrigins::PerFile(map) => *map.get(file)?,
                };
                (comment.version == before.version && *side == before.side).then_some(Side::Before)
            }
        }
    }
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
    fn new(diff: &Diff, comments: &'a [CommentState], origins: &CommentOrigins) -> Self {
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
                                comment,
                            });
                        }
                        _ => files[i].header.push(comment),
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
    use ulid::Ulid;
    use wiff_core::record::{Author, AuthorKind, CommentTarget, Confidence};
    use wiff_core::review::CommentState;
    use wiff_diff::{Diff, FileStatus, LineKind, Side};

    use super::testutil::{dump, file, ln, theme};
    use super::{CommentRegion, DiffView, ViewLayout};

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
            resolved_by: None,
            deleted: false,
            deleted_by: None,
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
        for n in 2..=6 {
            lines.push((LineKind::Context, format!("    let v{n} = {n};"), n));
        }
        lines.push((LineKind::Added, "    let w = 7;".to_string(), 7));
        let borrowed: Vec<(LineKind, &str, u32)> =
            lines.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect();
        let diff = Diff {
            files: vec![file("src/lib.rs", FileStatus::Modified, &borrowed)],
        };
        let doc = DiffView::new(theme()).unwrap().render(&diff);
        let markers: Vec<Line<'static>> = doc.folds.iter().map(|f| f.marker.clone()).collect();
        wince::snapshot_str!(
            dump(&markers),
            "<#767b84|-|->          ▸ [3 unchanged lines]  fn draw() {\n"
        );
    }

    #[test]
    fn foldable_runs_keep_context_around_changes_and_fold_the_rest() {
        use LineKind::{Added as A, Context as C};
        // Leading, interior, and trailing runs of context, keeping one line on
        // each side of the two changes.
        let kinds = [C, C, C, A, C, C, C, C, C, A, C, C, C];
        let none = vec![false; kinds.len()];
        wince::assert_eq!(
            super::foldable_runs(&kinds, 1, &none),
            vec![0..2, 5..8, 11..13]
        );
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
            "<#8fa1b3|-|->wez (human)<#767b84|-|->  press e to edit  r to resolve  d to delete  tab to expand/collapse\n",
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
            "<#8fa1b3|-|->wez (human)<#767b84|-|->  press e to edit  r to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->why 2?\n",
            "\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
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
            .map(|cell| cell.map_or('.', |c| c.glyph))
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
            .map(|cell| cell.map_or('.', |c| c.glyph))
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
            "<#8fa1b3|-|->wez (human)<#a3be8c|-|-> [draft]<#767b84|-|->  press e to edit  r to resolve  d to delete  tab to expand/collapse\n",
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
            "<#8fa1b3|-|->opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to unresolve  d to delete  tab to expand/collapse\n",
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
            "<#8fa1b3|-|->wez (human)<#767b84|-|->  press e to edit  r to resolve  d to delete  tab to expand/collapse\n",
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
            "<#767b84|-|->          ▸ [2 unchanged lines]  ctx02\n",
            "<#767b84|-|->          ▸ [7 unchanged lines]  ctx16\n",
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
            "<#8fa1b3|-|->dev (human)<#d08770|-|-> [outdated]<#767b84|-|->  press e to edit  r to resolve  d to delete  tab to expand/collapse\n",
            "<#c0c5ce|-|->stale\n",
            "\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;\n",
        );
    }
}
