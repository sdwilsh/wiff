//! The scrolling review view: a cursor over a rendered [`Document`].
//!
//! The app owns the rendered diff and the viewport into it: which row the
//! cursor is on and which row is at the top of the screen. It consumes the
//! navigation [`Action`]s (line, page, file, and hunk movement, and toggling a
//! fold) and reports any other action back to the host to handle. The cursor row
//! is washed with the theme's selection color so the reviewer can see where they
//! are, and single-line movement scrolls the view early to keep a margin of
//! rows on either side of the cursor rather than pinning it to an edge. The
//! cursor opens centered in the viewport so a review starts mid-screen. A status
//! line names the file the cursor is in and how far through the view it sits.
//!
//! Long runs of unchanged lines are folded away: the document carries the
//! foldable runs, and the app keeps each one collapsed until the reviewer
//! expands it, so the cursor and viewport move over a view that reflects what is
//! actually shown rather than every underlying row.

use std::collections::HashMap;
use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;
use ulid::Ulid;
use wiff_core::LineOrigin;
use wiff_core::draft::EffectiveComment;
use wiff_core::record::{CommentTarget, Confidence, RecordBody};
use wiff_core::review::CommentState;
use wiff_diff::{Diff, LineNo, LiveHighlighter, Rgb, Side};

use crate::action::Action;
use crate::compose::{Compose, ComposeKind, Scroll};
use crate::exit::{Exit, ExitDefault, ExitPlan, plan_exit};
use crate::help::{Help, HelpColors};
use crate::key::{Key, KeyPress};
use crate::keymap::{Keymap, Resolution};
use crate::picker::{Picker, PickerColors, PickerRow, RowSpan};
use crate::render::{
    BoxId, COLUMN_DIVIDER, COLUMN_GUTTER_WIDTH, DEFAULT_SIDE_BY_SIDE_MIN_WIDTH, DiffMode, Document,
    LayoutMode, RAIL_COLUMN, RAIL_TEE, RailCell, RowKind, ViewLayout, color, column_bounds,
    divider_column, rail_column,
};
use crate::review::{CommentSync, Review};
use crate::search::{Direction, Matcher, Search, SearchInput};
use crate::theme::{Theme, legible_over};

/// The result of feeding an [`Action`] to the [`App`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// The app handled the action; the view may have moved.
    Handled,
    /// The action is not one the app handles; the host should act on it.
    Passed(Action),
}

/// The viewport split around the inline comment editor while composing: the
/// document lines above the editor, the editor box itself, and the lines below
/// it, in draw order down the screen.
pub struct ComposeView {
    /// The document lines shown above the editor.
    pub above: Vec<Line<'static>>,
    /// The editor's border block, drawn around its rows.
    pub editor_block: Block<'static>,
    /// The editor's visible interior rows, wrapped and scrolled to fit the box.
    pub editor_rows: Vec<Line<'static>>,
    /// The cursor's (column, row) within `editor_rows`, for placing the terminal
    /// cursor inside the box; `None` when the cursor has scrolled out of view.
    pub editor_cursor: Option<(u16, u16)>,
    /// Present when the body overflows the box; `None` when it fits.
    pub editor_scroll: Option<Scroll>,
    /// The document lines shown below the editor.
    pub below: Vec<Line<'static>>,
    /// The side-by-side column the editor box is scoped to, or `None` when it
    /// spans the full width.
    pub column: Option<ComposeColumn>,
}

/// The placement of a side-by-side-scoped inline editor: where its box sits
/// horizontally and the divider rule drawn beside it, all as column offsets from
/// the view's left edge.
pub struct ComposeColumn {
    /// The box's left edge.
    pub x: u16,
    /// The box width.
    pub width: u16,
    /// The divider's column offset from the view's left edge.
    pub divider: u16,
    /// The divider glyph color.
    pub divider_fg: Rgb,
    /// The background color of the band outside the editor's column.
    pub background: Rgb,
}

/// Where the cursor sits while the editor floats detached from its anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetachFocus {
    /// The cursor is back in the floating editor, typing; the diff is frozen.
    Inside,
    /// The cursor roams the diff; the editor floats out of the way.
    Outside,
}

/// The direction of an arrow key that nudges focus out of the floating editor
/// and back onto the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nudge {
    Up,
    Down,
}

/// A frame of the detached editor, overlaid on the diff once the ordinary
/// document has been drawn behind it.
pub struct FloatView {
    /// The document row the box's top border sits on. It tracks the anchor as
    /// the diff scrolls, then comes to rest against the top or bottom edge once
    /// the anchor scrolls out of the viewport.
    pub top_row: u16,
    /// The color to fill behind the floating box before drawing it.
    pub background: Rgb,
    /// The editor's border block, drawn around its rows.
    pub editor_block: Block<'static>,
    /// The editor's visible interior rows, wrapped and scrolled to fit the box.
    pub editor_rows: Vec<Line<'static>>,
    /// The cursor's (column, row) within `editor_rows`, present only while the
    /// cursor is in the editor; `None` while it roams the diff.
    pub editor_cursor: Option<(u16, u16)>,
    /// Present when the body overflows the box; `None` when it fits.
    pub editor_scroll: Option<Scroll>,
    /// Blank columns held between each side of the box's border and the cleared
    /// area's edge. Zero when a narrower box would wrap the body to a taller box.
    pub inset: u16,
    /// The tint the chrome shows at `cursor_offset`.
    pub cursor_bg: Rgb,
    /// The box row, counting the borders, the roaming diff cursor sits behind;
    /// `None` while the cursor is in the editor or clear of the box.
    pub cursor_offset: Option<u16>,
}

/// Which family of rows a jump seeks: file, hunk, or comment headers.
#[derive(Debug, Clone, Copy)]
enum Landmark {
    File,
    Hunk,
    Comment,
}

impl Landmark {
    /// Whether `kind` is a row this landmark jumps between.
    fn matches(self, kind: &RowKind) -> bool {
        matches!(
            (self, kind),
            (Landmark::File, RowKind::FileHeader)
                | (Landmark::Hunk, RowKind::HunkHeader { .. })
                | (Landmark::Comment, RowKind::CommentHeader { .. })
        )
    }
}

/// One line of the current view: a document row, or a collapsed fold shown as
/// its marker line.
enum ViewRow {
    /// A row index into [`Document::rows`] and [`Document::lines`].
    Row(usize),
    /// A fold index into [`Document::folds`], collapsed to its marker.
    Fold(usize),
}

/// Where the cursor sat before the document was rebuilt, named the way the
/// document's own rows are so it can return to the same place afterward. Built
/// from the [`RowKind`] under the cursor, it survives a rebuild that renumbers
/// rows.
struct CursorSpot {
    /// The path of the file the cursor was in.
    file: String,
    /// The row within, or across, that file to return to.
    place: SpotPlace,
}

/// The kind of row a [`CursorSpot`] returns the cursor to, one variant per
/// family of row the cursor can rest on, mirroring the [`RowKind`] it was built
/// from.
#[derive(Clone, Copy)]
enum SpotPlace {
    /// A content line, returned to by side and number, or the nearest surviving
    /// line on that side when the exact line is gone.
    Line(Side, LineNo),
    /// A comment box, returned to by identity wherever its header now renders,
    /// or the file's header when the comment is gone.
    Comment(BoxId),
    /// A structural row -- a file or hunk header, or the review summary -- with
    /// no finer anchor than the file header.
    Header,
}

/// What the review's left side should show: an earlier version's after-content
/// as the reference point, or the latest version's own diff against its
/// baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareRequest {
    /// Compare the latest version against version `k`'s after-content.
    Version(u32),
    /// Return to the latest version's own captured diff.
    Latest,
}

/// A captured version in the modal list.
struct VersionRow {
    /// The text shown for the version in the list.
    label: String,
    /// The comparison choosing this row records.
    request: CompareRequest,
}

impl VersionRow {
    /// Box a row labeled `label` that records `request` when chosen.
    fn boxed(label: impl Into<String>, request: CompareRequest) -> Box<dyn PickerRow<App>> {
        Box::new(Self {
            label: label.into(),
            request,
        })
    }
}

impl PickerRow<App> for VersionRow {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn activate(self: Box<Self>, app: &mut App) {
        app.pending_compare = Some(self.request);
    }
}

/// A choice in the launch-time refresh prompt.
struct RefreshRow {
    /// The text shown for the choice in the list.
    label: String,
    /// Whether choosing it asks the host to recapture the source.
    refresh: bool,
}

impl PickerRow<App> for RefreshRow {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn activate(self: Box<Self>, app: &mut App) {
        if self.refresh {
            app.pending_refresh = true;
        }
    }
}

/// A file in the modal list: its display path and the document row of its
/// header, so choosing it jumps the cursor to that file.
struct FileRow {
    path: String,
    row: usize,
}

impl PickerRow<App> for FileRow {
    fn label(&self) -> String {
        self.path.clone()
    }

    fn activate(self: Box<Self>, app: &mut App) {
        if let Some(index) = app.locate_document_row(self.row) {
            app.move_to(index);
        }
    }
}

/// A comment in the modal list: its status marker, location, author, and body
/// start as styled fragments, and its stable identity, so choosing it jumps the
/// cursor to that comment's header.
struct CommentRow {
    spans: Vec<RowSpan>,
    id: Ulid,
}

impl PickerRow<App> for CommentRow {
    fn label(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    fn styled(&self) -> Option<Vec<RowSpan>> {
        Some(self.spans.clone())
    }

    fn activate(self: Box<Self>, app: &mut App) {
        if let Some(index) = app
            .comment_header_row(BoxId::Comment(self.id))
            .and_then(|row| app.locate_document_row(row))
        {
            app.move_to(index);
        }
    }
}

/// The colors the comment picker paints its status markers with.
#[derive(Clone, Copy)]
struct CommentMarkerColors {
    /// The `*` on a comment with an uncommitted change.
    draft: Rgb,
    /// The `check` on a resolved comment and the text of a withdrawn one.
    muted: Rgb,
    /// The `!` on a comment whose anchor drifted off its lines.
    warn: Rgb,
}

/// The sort key grouping comments in the picker: draft first, then open,
/// resolved, and withdrawn. A comment with an uncommitted change reads as a
/// draft whatever else is true of it.
fn comment_bucket(comment: &CommentState, pending: bool) -> u8 {
    if pending {
        0
    } else if comment.deleted {
        3
    } else if comment.resolved {
        2
    } else {
        1
    }
}

/// Where a comment is attached, for the picker's location column: `file:line`
/// or `file:start-end` for a line range, the bare path for a whole file, and
/// `review` for a comment on the review overall.
fn comment_location(target: &CommentTarget) -> String {
    match target {
        CommentTarget::Lines {
            file,
            start_line,
            end_line,
            ..
        } => {
            if start_line == end_line {
                format!("{file}:{}", start_line.get())
            } else {
                format!("{file}:{}-{}", start_line.get(), end_line.get())
            }
        }
        CommentTarget::File { file } => file.clone(),
        CommentTarget::Review => "review".to_string(),
        CommentTarget::Comment { .. } => "reply".to_string(),
    }
}

/// The first non-blank line of a comment body, trimmed, for the picker's
/// preview column.
fn comment_preview(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

/// A color theme in the modal list, named by its syntax theme, so choosing it
/// recolors the whole view to match.
struct ThemeRow {
    name: String,
}

impl PickerRow<App> for ThemeRow {
    fn label(&self) -> String {
        self.name.clone()
    }

    fn activate(self: Box<Self>, app: &mut App) {
        if let Some(theme) = Theme::named(&self.name) {
            app.apply_theme(&theme);
        }
    }
}

/// A way to leave in the modal list: its label and the outcome it settles on,
/// so choosing it resolves the quit the host is waiting on.
struct ExitRow {
    label: String,
    exit: Exit,
}

impl PickerRow<App> for ExitRow {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn activate(self: Box<Self>, app: &mut App) {
        app.exit = Some(self.exit);
    }
}

/// A live linewise selection while the reviewer marks a range to anchor a
/// comment to. Held within one file and one side; the anchor stays where the
/// selection began and the head follows the cursor as it moves.
struct Selection {
    /// The file the selection is confined to, indexing [`Document::files`].
    file: usize,
    /// The side the selected line numbers belong to.
    side: Side,
    /// The line the selection began on.
    anchor: LineNo,
    /// The line the cursor last extended the selection to.
    head: LineNo,
}

impl Selection {
    /// The inclusive line-number range the selection spans, low to high.
    fn range(&self) -> (LineNo, LineNo) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether the selection covers content line `(file, side, lineno)`.
    fn covers(&self, file: usize, side: Side, lineno: LineNo) -> bool {
        let (start, end) = self.range();
        self.file == file && self.side == side && start <= lineno && lineno <= end
    }
}

/// The review view over a rendered diff.
pub struct App {
    document: Document,
    /// The review being edited, present when the app can author comments. When
    /// absent the app is a read-only viewport and editing actions pass through.
    review: Option<Review>,
    /// The inline comment editor, present while authoring or revising a comment.
    compose: Option<Compose>,
    /// Where the cursor sits while the open editor floats detached from its
    /// anchor, present only while it floats; `None` while it renders inline.
    detach: Option<DetachFocus>,
    /// Whether an arrow past the editor's top or bottom row detaches it, from
    /// the reviewer's configured default.
    nudge_to_detach: bool,
    /// The chosen way to leave, set once a quit resolves, which the host reads
    /// to end the loop.
    exit: Option<Exit>,
    /// The open modal list, present while the reviewer is choosing from it.
    picker: Option<Picker<App>>,
    /// The open help overlay, present while the reviewer is reading it.
    help: Option<Help>,
    /// The version comparison the reviewer chose from the picker, present until
    /// the host takes it to reconstruct the diff.
    pending_compare: Option<CompareRequest>,
    /// The comparison a cancel of the open picker records, present for the
    /// post-refresh picker where cancelling keeps the reviewer's pre-refresh
    /// perspective; absent for pickers whose cancel does nothing.
    compare_on_cancel: Option<CompareRequest>,
    /// Whether the reviewer chose to refresh from the launch prompt, present
    /// until the host reads it to trigger a recapture.
    pending_refresh: bool,
    /// The default a quit resolves to, from the host's configured on-exit policy.
    exit_default: ExitDefault,
    /// Whether each of the document's folds is currently collapsed.
    collapsed: Vec<bool>,
    /// Whether each comment's body is currently collapsed, keyed by its stable
    /// identity so the state survives a document rebuild after a refresh.
    comment_collapsed: HashMap<BoxId, bool>,
    /// Whether every comment is dropped from the view, leaving only the code, so
    /// rounds of review annotation do not crowd out the diff.
    comments_hidden: bool,
    /// The live linewise selection, present while the reviewer marks a range to
    /// anchor a comment to.
    selection: Option<Selection>,
    /// The visible lines in order, resolved from the collapse state.
    view: Vec<ViewRow>,
    cursor: usize,
    top: usize,
    height: usize,
    /// The viewport width the document was last rendered for, so comment bodies
    /// wrap to it. Zero until the first draw supplies a real width.
    width: usize,
    /// Whether diff content lines wrap to the viewport width rather than being
    /// clipped at the edge, toggled by [`Action::ToggleWrap`].
    wrap: bool,
    /// The reviewer's chosen diff layout, resolved against the width each render.
    diff_mode: DiffMode,
    /// The minimum viewport width at which `DiffMode::Auto` selects side-by-side.
    side_by_side_min_width: usize,
    /// Columns per tab stop for the comment editor.
    tab_width: usize,
    /// Whether the initial cursor has been centered in the viewport, which
    /// happens once the first real height is known.
    positioned: bool,
    /// The background the whole view fills with, so the theme reads coherently
    /// over the terminal's own background.
    background: Rgb,
    /// The syntax theme currently in effect, so the theme picker can mark and
    /// open on the active choice.
    theme_name: String,
    cursor_bg: Rgb,
    search_match_bg: Rgb,
    status_fg: Rgb,
    status_bg: Rgb,
    /// The color of the rule dividing the two side-by-side columns.
    divider_fg: Rgb,
    /// The border color of the inline comment editor.
    compose_border: Rgb,
    /// The active bindings, resolving a press to an action. In the review view
    /// the host's own loop resolves keys; the app keeps the map to resolve the
    /// inline editor's own submit and cancel keys while composing.
    keymap: Keymap,
    /// The colors the modal list paints with.
    picker_colors: PickerColors,
    /// The colors the help overlay paints with.
    help_colors: HelpColors,
    /// The colors the comment picker paints its status markers with.
    comment_marker_colors: CommentMarkerColors,
    /// A transient note shown in the status line until the next key press, used
    /// to report the tally of a refresh.
    message: Option<String>,
    /// The open search prompt, present while the reviewer is typing a pattern.
    search: Option<Search>,
    /// The last accepted search, so `search_next` and `search_prev` can repeat
    /// it after the prompt closes.
    last_search: Option<(String, Direction)>,
    /// The accepted search shown in the status bar until the reviewer's next
    /// unrelated action: its pattern and direction, so the bar names the term,
    /// the repeat keys, and the live match tally.
    active_search: Option<(String, Direction)>,
    /// A note appended to the accepted search bar when a repeat wraps past an
    /// end of the document, shown alongside the tally rather than replacing it
    /// so the wrap is reported without wiping the position.
    search_note: Option<String>,
}

impl App {
    /// Build the view over `document`, showing `height` rows, selecting rows
    /// with `theme`'s cursor color. Every fold starts collapsed.
    pub fn new(document: Document, height: usize, theme: &Theme) -> Self {
        let collapsed = vec![true; document.folds.len()];
        let comment_collapsed = document
            .comments
            .iter()
            .map(|region| (region.id, region.collapsed_default))
            .collect();
        let mut app = Self {
            document,
            review: None,
            compose: None,
            detach: None,
            nudge_to_detach: true,
            exit: None,
            picker: None,
            help: None,
            pending_compare: None,
            compare_on_cancel: None,
            pending_refresh: false,
            exit_default: ExitDefault::Prompt,
            collapsed,
            comment_collapsed,
            comments_hidden: false,
            selection: None,
            view: Vec::new(),
            cursor: 0,
            top: 0,
            height,
            width: 0,
            wrap: false,
            diff_mode: DiffMode::default(),
            side_by_side_min_width: DEFAULT_SIDE_BY_SIDE_MIN_WIDTH,
            tab_width: wiff_diff::DEFAULT_TAB_WIDTH,
            positioned: false,
            background: theme.background,
            theme_name: theme.syntax_theme.clone(),
            cursor_bg: theme.cursor_bg,
            search_match_bg: theme.search_match_bg,
            status_fg: theme.status_fg,
            status_bg: theme.status_bg,
            divider_fg: theme.gutter_fg,
            compose_border: theme.comment_draft_fg,
            keymap: Keymap::defaults(),
            picker_colors: PickerColors {
                border: theme.review_fg,
                background: theme.background,
                selected_bg: theme.cursor_bg,
                text: theme.comment_fg,
                hint: theme.fold_fg,
            },
            help_colors: HelpColors {
                border: theme.review_fg,
                background: theme.background,
                keys: theme.comment_author_fg,
                text: theme.comment_fg,
                hint: theme.fold_fg,
            },
            comment_marker_colors: CommentMarkerColors {
                draft: theme.comment_draft_fg,
                muted: theme.comment_flag_fg,
                warn: theme.comment_warn_fg,
            },
            message: None,
            search: None,
            last_search: None,
            active_search: None,
            search_note: None,
        };
        app.rebuild_view();
        app
    }

    /// Build the view over `review`, rendering its current state and retaining
    /// it so buffered edits re-render in place. Otherwise like [`new`](App::new).
    pub fn reviewing(review: Review, height: usize, theme: &Theme) -> Self {
        // No width is known yet, so the document renders unwrapped; the first
        // draw's `set_width` reflows it to fit the terminal.
        let mut app = Self::new(review.document(ViewLayout::default()), height, theme);
        app.review = Some(review);
        app
    }

    /// Set the default a quit resolves to, from the host's on-exit policy.
    pub fn with_exit_default(mut self, default: ExitDefault) -> Self {
        self.exit_default = default;
        self
    }

    /// Start with diff content wrapped to the viewport width rather than clipped,
    /// from the reviewer's configured default. A width is not known yet, so this
    /// only records the choice; the first draw reflows to it.
    pub fn with_wrap_content(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Adopt the reviewer's diff layout `mode` and the `min_width` at which auto
    /// mode switches to side-by-side.
    pub fn with_diff_mode(mut self, mode: DiffMode, min_width: usize) -> Self {
        self.diff_mode = mode;
        self.side_by_side_min_width = min_width;
        self
    }

    /// Set the comment editor's tab width.
    pub fn with_tab_width(mut self, tab_width: usize) -> Self {
        self.tab_width = tab_width;
        self
    }

    /// Adopt the reviewer's `keymap`, so the inline editor's submit and cancel
    /// keys match their configured bindings.
    pub fn with_keymap(mut self, keymap: Keymap) -> Self {
        self.keymap = keymap;
        self
    }

    /// Set whether an arrow past the top or bottom of the open editor detaches
    /// it, from the reviewer's configured default.
    pub fn with_nudge_to_detach(mut self, nudge: bool) -> Self {
        self.nudge_to_detach = nudge;
        self
    }

    /// The editor border hint naming the keys that submit and cancel a comment,
    /// resolved from the active keymap so it shows the reviewer's own bindings.
    /// While the editor floats, the detach key is named too, before the cancel
    /// key so the cancel hint is the one that truncates when the title
    /// overflows: it reads `edit` while the cursor roams the diff and
    /// `navigate` while it is back in the editor, naming what the key does next.
    fn editor_hint(&self) -> String {
        let submit = self
            .keymap
            .primary_label(Action::SubmitComment)
            .unwrap_or_default();
        let cancel = self
            .keymap
            .primary_label(Action::CancelComment)
            .unwrap_or_default();
        let Some(focus) = self.detach else {
            return format!("{submit} submit  {cancel} cancel");
        };
        let detach = self
            .keymap
            .primary_label(Action::DetachEditor)
            .unwrap_or_default();
        let verb = match focus {
            DetachFocus::Inside => "navigate",
            DetachFocus::Outside => "edit",
        };
        format!("{submit} submit  [{detach} {verb}]  {cancel} cancel")
    }

    /// An incremental markdown highlighter for the inline editor, over the
    /// review's current theme. Composing only starts with a review attached, so
    /// its absence here is a bug.
    fn editor_highlighter(&self) -> LiveHighlighter {
        self.review
            .as_ref()
            .expect("composing requires an attached review")
            .live_highlighter("markdown")
    }

    /// Take the pending draft records to be committed to the session log,
    /// emptying the buffer. Empty when no review is attached.
    pub fn take_drafts(&mut self) -> Vec<RecordBody> {
        self.review
            .as_mut()
            .map(Review::take_drafts)
            .unwrap_or_default()
    }

    /// The append events that would persist the pending drafts, without emptying
    /// the buffer. Empty when no review is attached.
    pub fn draft_records(&self) -> Vec<RecordBody> {
        self.review
            .as_ref()
            .map(Review::draft_records)
            .unwrap_or_default()
    }

    /// Empty the draft buffer after its records were committed to the log.
    pub fn clear_drafts(&mut self) {
        if let Some(review) = self.review.as_mut() {
            review.clear_drafts();
        }
    }

    /// The view row the cursor is on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The first visible view row.
    pub fn top(&self) -> usize {
        self.top
    }

    /// Resize the viewport to `height` rows, keeping the cursor visible. The
    /// first time a real height is applied the cursor is centered in the
    /// viewport, so a review opens with the cursor mid-screen and a single move
    /// scrolls at once rather than walking to an edge.
    pub fn set_height(&mut self, height: usize) {
        self.height = height;
        if !self.positioned && height > 0 {
            self.positioned = true;
            self.cursor = (height / 2).min(self.last_view());
        }
        self.scroll_into_view();
    }

    /// Set the viewport `width` and, when it changes, reflow the review's
    /// document so comment bodies and any wrapped content fit the new width. A
    /// read-only view has no review to re-render, so it only records the width.
    pub fn set_width(&mut self, width: usize) {
        if width == self.width {
            return;
        }
        self.width = width;
        self.reflow();
    }

    /// The layout the document renders to: the current width and whether diff
    /// content wraps to it.
    fn layout(&self) -> ViewLayout {
        ViewLayout {
            width: self.width,
            wrap_content: self.wrap,
            mode: self
                .diff_mode
                .resolve(self.width, self.side_by_side_min_width),
        }
    }

    /// Re-render the review's document for the current width and wrap setting,
    /// keeping the cursor's spot and each comment's collapse state. A read-only
    /// view has no review to re-render, so it does nothing.
    fn reflow(&mut self) {
        if self.review.is_none() {
            return;
        }
        let spot = self.cursor_spot();
        let document = self
            .review
            .as_ref()
            .expect("review present")
            .document(self.layout());
        self.adopt_document(document);
        self.restore_spot(spot);
    }

    /// Swap in a freshly rendered `document`, keeping each fold's collapse state
    /// while the fold structure lines up and resetting it to collapsed when the
    /// fold count has moved. A comment inside a long unchanged run splits the
    /// fold around it, so the count can change even when the diff has not.
    /// Comment collapse state is reconciled by identity. The caller places the
    /// cursor afterward.
    fn adopt_document(&mut self, document: Document) {
        if document.folds.len() != self.collapsed.len() {
            self.collapsed = vec![true; document.folds.len()];
        }
        self.reconcile_comment_collapse(&document);
        self.document = document;
        self.rebuild_view();
    }

    /// Toggle whether diff content wraps to the viewport width or is clipped at
    /// the edge, reflowing the document to the new choice. Passes through when no
    /// review is being edited.
    fn toggle_wrap(&mut self) -> Update {
        if self.review.is_none() {
            return Update::Passed(Action::ToggleWrap);
        }
        self.wrap = !self.wrap;
        self.reflow();
        Update::Handled
    }

    /// Switch the active diff layout to `mode`, re-rendering at the current width
    /// and keeping the cursor on the same content line. The switch is
    /// session-local; it is not written back to config. Passes through when no
    /// review is being edited.
    fn set_diff_mode(&mut self, mode: DiffMode) -> Update {
        if self.review.is_none() {
            return Update::Passed(Self::diff_mode_action(mode));
        }
        if mode == self.diff_mode {
            return Update::Handled;
        }
        self.diff_mode = mode;
        self.reflow();
        Update::Handled
    }

    /// The action that selects diff `mode`, used when passing the intent back to
    /// the host on a read-only view.
    fn diff_mode_action(mode: DiffMode) -> Action {
        match mode {
            DiffMode::Auto => Action::DiffModeAuto,
            DiffMode::SideBySide => Action::DiffModeSideBySide,
            DiffMode::Unified => Action::DiffModeUnified,
            DiffMode::OnlyAfter => Action::DiffModeOnlyAfter,
        }
    }

    /// Handle a navigation action, or pass any other action back to the host.
    pub fn update(&mut self, action: Action) -> Update {
        // Any action clears a lingering refresh note or search tally so neither
        // outstays the reviewer's next move.
        self.message = None;
        self.active_search = None;
        self.search_note = None;
        // Movement extends a live selection; other actions abandon it, except
        // the ones that start, consume, or cancel it below.
        let extends = Self::extends_selection(action);
        if !(extends
            || matches!(
                action,
                Action::SelectLines | Action::AddComment | Action::CancelComment
            ))
        {
            self.selection = None;
        }
        match action {
            Action::LineDown => self.move_to(self.cursor + 1),
            Action::LineUp => self.move_to(self.cursor.saturating_sub(1)),
            Action::PageDown => self.page_down(),
            Action::PageUp => self.page_up(),
            Action::Top => self.move_to(0),
            Action::Bottom => self.move_to(self.last_view()),
            Action::NextFile => self.jump_forward(Landmark::File),
            Action::PrevFile => self.jump_backward(Landmark::File),
            Action::NextHunk => self.jump_forward(Landmark::Hunk),
            Action::PrevHunk => self.jump_backward(Landmark::Hunk),
            Action::NextComment => self.jump_forward(Landmark::Comment),
            Action::PrevComment => self.jump_backward(Landmark::Comment),
            Action::ToggleFold => self.toggle_fold(),
            Action::ToggleComment => self.toggle_comment(),
            Action::ToggleWrap => return self.toggle_wrap(),
            Action::DiffModeAuto => return self.set_diff_mode(DiffMode::Auto),
            Action::DiffModeSideBySide => return self.set_diff_mode(DiffMode::SideBySide),
            Action::DiffModeUnified => return self.set_diff_mode(DiffMode::Unified),
            Action::DiffModeOnlyAfter => return self.set_diff_mode(DiffMode::OnlyAfter),
            Action::HideComments => self.toggle_comments_hidden(),
            Action::PickFile => self.open_file_picker(),
            Action::PickComment => self.open_comment_picker(),
            Action::PickTheme => self.open_theme_picker(),
            Action::Help => self.open_help(),
            Action::CompareVersions => self.open_compare_picker(),
            Action::ResolveComment => return self.resolve_comment(),
            Action::DeleteComment => return self.delete_comment(),
            Action::SetVerdict => return self.set_verdict(),
            Action::SelectLines => self.start_selection(),
            Action::CancelComment => return self.cancel_selection(),
            Action::AddComment => return self.start_add_comment(),
            Action::ReplyComment => return self.start_reply_comment(),
            Action::EditComment => return self.start_edit_comment(),
            Action::SearchForward => self.start_search(Direction::Forward),
            Action::SearchBackward => self.start_search(Direction::Backward),
            Action::SearchNext => self.repeat_search(false),
            Action::SearchPrev => self.repeat_search(true),
            quit @ (Action::Quit | Action::QuitKeep | Action::QuitRemove) => {
                return self.request_exit(quit);
            }
            other => return Update::Passed(other),
        }
        if extends {
            self.extend_selection_to_cursor();
        }
        Update::Handled
    }

    /// Whether `action` moves the cursor, so a live selection extends its head
    /// to follow rather than being abandoned.
    fn extends_selection(action: Action) -> bool {
        matches!(
            action,
            Action::LineDown
                | Action::LineUp
                | Action::PageDown
                | Action::PageUp
                | Action::Top
                | Action::Bottom
                | Action::NextFile
                | Action::PrevFile
                | Action::NextHunk
                | Action::PrevHunk
                | Action::NextComment
                | Action::PrevComment
        )
    }

    /// The file, side, and line number of the content line at view row `index`,
    /// or `None` when that row is not an addressable content line.
    fn content_at(&self, index: usize) -> Option<(usize, Side, LineNo)> {
        let ViewRow::Row(row) = *self.view.get(index)? else {
            return None;
        };
        let (side, lineno) = self.document.rows[row].kind.content_addr()?;
        Some((self.document.rows[row].file, side, lineno))
    }

    /// The first view row showing content line `(file, side, lineno)`, or `None`
    /// when that line is not currently in view (folded away or off this side).
    fn content_row_in_view(&self, file: usize, side: Side, lineno: LineNo) -> Option<usize> {
        let index =
            (0..self.view.len()).find(|&i| self.content_at(i) == Some((file, side, lineno)))?;
        Some(self.content_line_top(index))
    }

    /// Whether view row `index` is a content line inside the live selection.
    fn row_selected(&self, index: usize) -> bool {
        let Some(sel) = &self.selection else {
            return false;
        };
        self.content_at(index)
            .is_some_and(|(file, side, lineno)| sel.covers(file, side, lineno))
    }

    /// Start a linewise selection at the cursor's content line, replacing any
    /// selection already open. Does nothing on a row that is not a content line.
    fn start_selection(&mut self) {
        if let Some((file, side, lineno)) = self.content_at(self.cursor) {
            self.selection = Some(Selection {
                file,
                side,
                anchor: lineno,
                head: lineno,
            });
        }
    }

    /// Extend the open selection's head to the cursor's content line. A move that
    /// leaves the selection's file abandons it, since a selection is held within
    /// one file; within the file, a move onto the opposite side or a non-content
    /// row leaves the head where it was.
    fn extend_selection_to_cursor(&mut self) {
        let Some((sel_file, sel_side)) = self.selection.as_ref().map(|sel| (sel.file, sel.side))
        else {
            return;
        };
        if self.cursor_file() != Some(sel_file) {
            self.selection = None;
            return;
        }
        if let Some((file, side, lineno)) = self.content_at(self.cursor)
            && file == sel_file
            && side == sel_side
            && let Some(sel) = self.selection.as_mut()
        {
            sel.head = lineno;
        }
    }

    /// Cancel a live selection. Passes [`Action::CancelComment`] back to the host
    /// when there is no selection to cancel, so the key keeps its other meaning.
    fn cancel_selection(&mut self) -> Update {
        if self.selection.take().is_some() {
            Update::Handled
        } else {
            Update::Passed(Action::CancelComment)
        }
    }

    /// Toggle the resolved state of the comment the cursor is on, buffering the
    /// change and re-rendering. Passes through when no review is being edited;
    /// does nothing when the cursor is not on a comment.
    fn resolve_comment(&mut self) -> Update {
        if self.review.is_none() {
            return Update::Passed(Action::ResolveComment);
        }
        if let Some(BoxId::Comment(comment)) = self.comment_at_cursor() {
            if let Some(review) = self.review.as_mut() {
                review.toggle_resolved(comment);
            }
            self.rerender();
            self.focus_comment(BoxId::Comment(comment));
        }
        Update::Handled
    }

    /// Toggle the deleted state of the comment the cursor is on, buffering the
    /// change and re-rendering. Deleting collapses the comment to its header,
    /// shown as withdrawn, so an accidental delete is visible and can be undone
    /// with the same action; restoring expands it again. Passes through when no
    /// review is being edited; does nothing when the cursor is not on a comment.
    fn delete_comment(&mut self) -> Update {
        if self.review.is_none() {
            return Update::Passed(Action::DeleteComment);
        }
        if let Some(BoxId::Comment(comment)) = self.comment_at_cursor() {
            let id = BoxId::Comment(comment);
            if let Some(review) = self.review.as_mut() {
                let deleted = review.toggle_deleted(comment);
                self.comment_collapsed.insert(id, deleted);
            }
            self.rerender();
            self.focus_comment(id);
        }
        Update::Handled
    }

    /// Cycle your verdict on the comment the cursor is on, buffering the change
    /// and re-rendering. A verdict is the comment author's own, so this only
    /// acts on a comment the review's author wrote. Passes through when no
    /// review is being edited; does nothing when the cursor is not on such a
    /// comment.
    fn set_verdict(&mut self) -> Update {
        if self.review.is_none() {
            return Update::Passed(Action::SetVerdict);
        }
        if let Some(BoxId::Comment(comment)) = self.comment_at_cursor()
            && let Some(review) = self.review.as_mut()
            && review.cycle_disposition(comment)
        {
            self.rerender();
            self.focus_comment(BoxId::Comment(comment));
        }
        Update::Handled
    }

    /// Open the inline editor to author a comment at the cursor, deriving its
    /// target from the row the cursor is on. Passes through when no review is
    /// being edited; does nothing on a row that anchors no comment.
    fn start_add_comment(&mut self) -> Update {
        if self.review.is_none() {
            return Update::Passed(Action::AddComment);
        }
        if let Some((target, anchor, label)) = self.add_target_at_cursor() {
            let highlighter = self.editor_highlighter();
            self.compose = Some(Compose::new(
                ComposeKind::Add(target),
                anchor,
                "",
                label,
                self.compose_border,
                highlighter,
                self.tab_width,
            ));
            // The selection has become the new comment's anchor, so retire it.
            self.selection = None;
        }
        Update::Handled
    }

    /// Open the inline editor to reply to the comment the cursor is on, forming
    /// a thread rooted at that comment. Passes through when no review is being
    /// edited; does nothing when the cursor is not on a comment or the comment
    /// is withdrawn, which cannot take a reply.
    fn start_reply_comment(&mut self) -> Update {
        let Some(review) = self.review.as_ref() else {
            return Update::Passed(Action::ReplyComment);
        };
        let Some(BoxId::Comment(parent)) = self.comment_at_cursor() else {
            return Update::Handled;
        };
        if !review.can_reply(parent) {
            return Update::Handled;
        }
        let anchor = self.skip_comment_rows(self.cursor);
        let highlighter = self.editor_highlighter();
        self.compose = Some(Compose::new(
            ComposeKind::Add(CommentTarget::Comment { id: parent }),
            anchor,
            "",
            "reply".to_string(),
            self.compose_border,
            highlighter,
            self.tab_width,
        ));
        Update::Handled
    }

    /// Open the inline editor to revise the comment the cursor is on, seeded
    /// with its current body. Passes through when no review is being edited;
    /// does nothing when the cursor is not on a comment.
    fn start_edit_comment(&mut self) -> Update {
        if self.review.is_none() {
            return Update::Passed(Action::EditComment);
        }
        // Editing the review summary writes the description while the review has
        // none: it draws no box of its own, so the first one is authored from
        // here. Once written it renders as its own row the cursor edits directly.
        if matches!(self.kind_at(self.cursor), Some(RowKind::ReviewSummary))
            && self.review.as_ref().is_some_and(Review::description_absent)
        {
            let anchor = self.skip_comment_rows(self.cursor + 1);
            let highlighter = self.editor_highlighter();
            self.compose = Some(Compose::new(
                ComposeKind::Edit(BoxId::Description),
                anchor,
                "",
                "edit description".to_string(),
                self.compose_border,
                highlighter,
                self.tab_width,
            ));
            return Update::Handled;
        }
        if let Some(id) = self.comment_at_cursor()
            && let Some(anchor) = self
                .comment_header_row(id)
                .and_then(|row| self.view_index_of_row(row))
        {
            let (body, label) = match id {
                BoxId::Comment(comment) => (
                    self.review
                        .as_ref()
                        .and_then(|review| review.comment_body(comment))
                        .unwrap_or_default(),
                    "edit comment",
                ),
                BoxId::Description => (
                    self.review
                        .as_ref()
                        .map(Review::description_body)
                        .unwrap_or_default(),
                    "edit description",
                ),
            };
            let highlighter = self.editor_highlighter();
            self.compose = Some(Compose::new(
                ComposeKind::Edit(id),
                anchor,
                &body,
                label.to_string(),
                self.compose_border,
                highlighter,
                self.tab_width,
            ));
        }
        Update::Handled
    }

    /// The target, anchor view row, and border label for a comment authored at
    /// the cursor: a line comment on a content row, a whole-file comment on a
    /// file header, or a review comment on the summary row. `None` on a row that
    /// anchors no comment.
    fn add_target_at_cursor(&self) -> Option<(CommentTarget, usize, String)> {
        // A live selection defines the target directly: a range comment whose
        // box sits above the span's first line, wherever the cursor rests.
        if let Some(sel) = &self.selection {
            let (start, end) = sel.range();
            let path = self.document.files.get(sel.file)?.clone();
            let anchor = self.content_row_in_view(sel.file, sel.side, start)?;
            let label = if start == end {
                format!("new comment  {path}:{}", start.get())
            } else {
                format!("new comment  {path}:{}-{}", start.get(), end.get())
            };
            let target = CommentTarget::Lines {
                file: path,
                side: sel.side,
                start_line: start,
                end_line: end,
            };
            return Some((target, anchor, label));
        }
        match self.kind_at(self.cursor)? {
            RowKind::ReviewSummary => Some((
                CommentTarget::Review,
                self.skip_comment_rows(self.cursor + 1),
                "new review comment".to_string(),
            )),
            RowKind::FileHeader => {
                let path = self.document.files.get(self.cursor_file()?)?.clone();
                let label = format!("new comment  {path}");
                let anchor = self.skip_comment_rows(self.cursor + 1);
                Some((CommentTarget::File { file: path }, anchor, label))
            }
            RowKind::Content { .. } => {
                let (side, lineno) = self.kind_at(self.cursor)?.content_addr()?;
                let path = self.document.files.get(self.cursor_file()?)?.clone();
                let label = format!("new comment  {path}:{}", lineno.get());
                let target = CommentTarget::Lines {
                    file: path,
                    side,
                    start_line: lineno,
                    end_line: lineno,
                };
                // A soft-wrapped line spans several rows sharing one anchor, so
                // the editor opens above the line's first row to match where the
                // submitted comment lands rather than the continuation the cursor
                // sits on.
                Some((target, self.content_line_top(self.cursor), label))
            }
            _ => None,
        }
    }

    /// The first view row of the content line shown at view row `index`: walking
    /// back over the continuation rows a soft-wrapped line breaks into, all of
    /// which share its side and line number. Returns `index` unchanged for a row
    /// that is not a content line.
    fn content_line_top(&self, index: usize) -> usize {
        let Some(kind) = self.kind_at(index) else {
            return index;
        };
        if kind.content_addr().is_none() {
            return index;
        }
        // A soft-wrapped line's continuation rows share its full row identity
        // (both columns), so walk back over rows with the same kind.
        let kind = kind.clone();
        let mut top = index;
        while top > 0 && self.kind_at(top - 1) == Some(&kind) {
            top -= 1;
        }
        top
    }

    /// Whether the inline comment editor is open, so the host routes raw key
    /// presses to it rather than resolving them into actions.
    pub fn composing(&self) -> bool {
        self.compose.is_some()
    }

    /// Feed a key press to the open editor: submit on the [`Action::SubmitComment`]
    /// binding, cancel on [`Action::CancelComment`] (confirming first when the
    /// body has unsaved changes), and otherwise let the editor handle the press.
    /// Only single-press bindings act, since the editor keeps no pending chord
    /// state. Does nothing when the editor is closed.
    pub fn compose_key(&mut self, press: KeyPress) {
        let Some(compose) = self.compose.as_mut() else {
            return;
        };
        if compose.confirming() {
            match press.key {
                Key::Char('y') | Key::Char('Y') => self.close_compose(),
                _ => compose.resume(),
            }
            return;
        }
        let outside = matches!(self.detach, Some(DetachFocus::Outside));
        if outside {
            self.compose_key_outside(press);
        } else {
            self.compose_key_editing(press);
        }
    }

    /// Route a press while the cursor is in the editor, whether anchored inline
    /// or floating detached. Submit and cancel leave; the detach binding, or an
    /// arrow past the editor's edge when nudging is enabled, floats the editor
    /// and hands the cursor to the diff; every other press edits the body.
    fn compose_key_editing(&mut self, press: KeyPress) {
        let width = self.width.saturating_sub(2);
        match self.keymap.resolve(std::slice::from_ref(&press)) {
            Resolution::Action(Action::SubmitComment) => self.submit_compose(),
            Resolution::Action(Action::CancelComment) => self.cancel_compose(),
            Resolution::Action(Action::DetachEditor) => self.detach_to_outside(),
            _ => {
                if let Some(nudge) = self.nudge_out_of_editor(&press, width) {
                    self.nudge_out(nudge, width);
                    return;
                }
                if let Some(compose) = self.compose.as_mut() {
                    compose.input(press, width);
                }
            }
        }
    }

    /// Route a press while the floating editor's cursor roams the diff. Submit
    /// and cancel still leave; navigation and search drive the diff; the edit or
    /// detach binding, or any other key, returns to the editor, a printable key
    /// inserting itself once back inside.
    fn compose_key_outside(&mut self, press: KeyPress) {
        match self.keymap.resolve(std::slice::from_ref(&press)) {
            Resolution::Action(Action::SubmitComment) => self.submit_compose(),
            Resolution::Action(Action::CancelComment) => self.cancel_compose(),
            Resolution::Action(Action::EditComment | Action::DetachEditor) => self.reenter_editor(),
            Resolution::Action(action) if is_navigation(action) => {
                self.update(action);
            }
            _ => {
                self.reenter_editor();
                if let (Key::Char(_), Some(compose)) = (press.key, self.compose.as_mut()) {
                    compose.input(press, self.width.saturating_sub(2));
                }
            }
        }
    }

    /// Whether `press` is an arrow key that would step past the editor's top or
    /// bottom row, returning the direction it detaches in. `None` when nudging
    /// is disabled, the press is not a vertical step, or the step stays within
    /// the editor.
    fn nudge_out_of_editor(&self, press: &KeyPress, width: usize) -> Option<Nudge> {
        if !self.nudge_to_detach {
            return None;
        }
        let compose = self.compose.as_ref()?;
        match nudge_direction(press)? {
            Nudge::Up if compose.at_first_visual_row(width) => Some(Nudge::Up),
            Nudge::Down if compose.at_last_visual_row(width) => Some(Nudge::Down),
            _ => None,
        }
    }

    /// Detach the editor and hand the cursor to the diff, placing it on the
    /// anchor line on the first detach. The editor keeps floating over the
    /// anchor and drifts to a screen edge only if the reviewer scrolls the
    /// anchor out of view.
    fn detach_to_outside(&mut self) {
        // Only the first detach moves the cursor. A later toggle from Inside
        // leaves it where it roamed to on the diff; the view cannot have
        // scrolled since, as navigation is frozen while the cursor is Inside.
        if self.detach.is_none() {
            let anchor = self.compose_anchor();
            self.move_to(anchor);
        }
        self.detach = Some(DetachFocus::Outside);
    }

    /// Detach in response to an arrow past the editor's edge and hand the cursor
    /// to the diff just past the box the way the arrow pointed. On the first
    /// detach the box floats where `compose_float` will place it, and the cursor
    /// steps to the diff row just past the border the arrow pointed at: the row
    /// above the top border going up, or below the bottom border going down,
    /// mirroring both edges. When that border already rests against the matching
    /// viewport edge there is no such row on screen, so the nudge does nothing
    /// rather than scrolling the anchor away from the box. A later nudge steps a
    /// single row from where the roaming cursor already sits.
    fn nudge_out(&mut self, nudge: Nudge, interior: usize) {
        if self.detach.is_none() {
            let box_height = self.float_box_height(interior);
            let anchor_row = self.compose_anchor().saturating_sub(self.top);
            let top_row = anchor_row.min(self.height.saturating_sub(box_height));
            let target_row = match nudge {
                Nudge::Up if top_row == 0 => return,
                Nudge::Up => top_row - 1,
                Nudge::Down if top_row + box_height >= self.height => return,
                Nudge::Down => top_row + box_height,
            };
            self.detach = Some(DetachFocus::Outside);
            self.move_to(self.top + target_row);
            return;
        }
        self.detach = Some(DetachFocus::Outside);
        match nudge {
            Nudge::Up => self.move_to(self.cursor.saturating_sub(1)),
            Nudge::Down => self.move_to(self.cursor + 1),
        }
    }

    /// The tallest interior the floating editor's body is shown at: about a
    /// third of the viewport, and at least one row, so a long body does not
    /// crowd out the diff while a short one still shrinks to fit.
    fn editor_max_interior(&self) -> usize {
        (self.height * 3 / 10).max(3).saturating_sub(2).max(1)
    }

    /// The height of the floating editor box, counting both borders, when its
    /// body wraps to `interior` columns.
    fn float_box_height(&self, interior: usize) -> usize {
        self.compose.as_ref().map_or(2, |compose| {
            compose
                .layout(interior, self.editor_max_interior())
                .rows
                .len()
                + 2
        })
    }

    /// Return the roaming cursor to the floating editor, freezing the diff where
    /// the reviewer left it. The editor's insertion point is untouched, so
    /// typing resumes where it left off.
    fn reenter_editor(&mut self) {
        if self.detach.is_some() {
            self.detach = Some(DetachFocus::Inside);
        }
    }

    /// The editor's anchor row, clamped to the view.
    fn compose_anchor(&self) -> usize {
        self.compose
            .as_ref()
            .map_or(0, |compose| compose.anchor().min(self.last_view()))
    }

    /// Cancel the open editor, confirming first when the body has unsaved
    /// changes so an accidental keystroke cannot discard work.
    fn cancel_compose(&mut self) {
        let Some(compose) = self.compose.as_mut() else {
            return;
        };
        if compose.is_dirty() {
            compose.begin_confirm();
        } else {
            self.close_compose();
        }
    }

    /// Close the editor, clearing any detached state.
    fn close_compose(&mut self) {
        self.compose = None;
        self.detach = None;
    }

    /// Commit the open editor's body to the review: a new comment or a revision.
    /// An empty body is discarded like a cancel, for both a comment and the
    /// description; a local edit never sets a blank description. Focuses the
    /// resulting comment.
    fn submit_compose(&mut self) {
        let Some(compose) = self.compose.take() else {
            return;
        };
        self.detach = None;
        let body = compose.body();
        if body.is_empty() {
            return;
        }
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let id = match compose.into_kind() {
            ComposeKind::Add(target) => BoxId::Comment(review.add_comment(target, body)),
            ComposeKind::Edit(id @ BoxId::Comment(comment)) => {
                review.edit_comment(comment, body);
                id
            }
            ComposeKind::Edit(id @ BoxId::Description) => {
                review.edit_description(body);
                id
            }
        };
        self.rerender();
        self.focus_comment(id);
    }

    /// The viewport split around the inline editor, when composing: the document
    /// lines above the editor, the editor widget, and the lines below it,
    /// together filling the viewport. The editor renders where its comment will,
    /// keeping the anchored code just below it. Each document line fills its row
    /// to `width` so its tint reaches the edge as it does outside the editor.
    /// `None` when not composing.
    pub fn compose_view(&self, width: usize) -> Option<ComposeView> {
        let compose = self.compose.as_ref()?;
        // A detached editor floats over the ordinary document render instead;
        // see `compose_float`.
        if self.detach.is_some() {
            return None;
        }
        if self.height == 0 {
            return None;
        }
        // In a side-by-side layout the editor stands in its target column, where
        // its rendered box will sit, rather than spanning both.
        let column_side = self.compose_column();
        let (editor_x, editor_width) = match column_side {
            Some(side) => column_bounds(width, side),
            None => (0, width),
        };
        // The border takes a column on each side; the body wraps into the rest.
        let interior = editor_width.saturating_sub(2);
        let max_interior = self.editor_max_interior();
        let editor = compose.layout(interior, max_interior);
        let editor_height = editor.rows.len() + 2;
        let doc_shown = self.height.saturating_sub(editor_height);
        let anchor = compose.anchor().min(self.view.len());
        // Leave the rows already above the anchor where they sit and let the
        // editor push the anchored code and everything below it down, so opening
        // the editor does not scroll the view out from under the reviewer. Only
        // when those rows would leave no room for the editor do we scroll up.
        let above_start = self.top.max(anchor.saturating_sub(doc_shown)).min(anchor);
        // When revising an existing comment, drop its rendered rows from the
        // view below the editor so the editor stands in its place rather than
        // sitting atop the box it is editing.
        let below_start = anchor
            + compose
                .editing()
                .map_or(0, |id| self.comment_rows(anchor, id));
        let below_count = doc_shown - (anchor - above_start);
        let below_end = (below_start + below_count).min(self.view.len());
        let compose_span = self.compose_rail_span();
        let above = (above_start..anchor)
            .map(|i| self.decorate(i, width, compose_span))
            .collect();
        let below = (below_start..below_end)
            .map(|i| self.decorate(i, width, compose_span))
            .collect();
        let column = column_side.map(|_| ComposeColumn {
            x: editor_x as u16,
            width: editor_width as u16,
            divider: divider_column(width) as u16,
            divider_fg: self.divider_fg,
            background: self.background,
        });
        Some(ComposeView {
            above,
            editor_block: compose.block(&self.editor_hint()),
            editor_rows: editor.rows,
            editor_cursor: editor.cursor,
            editor_scroll: editor.scroll,
            below,
            column,
        })
    }

    /// The side-by-side column the inline editor belongs in, derived from its
    /// target comment's placement. `None` for a full-width editor: a unified
    /// layout, or a review- or file-level comment that no column owns.
    fn compose_column(&self) -> Option<Side> {
        if self.document.mode != LayoutMode::SideBySide {
            return None;
        }
        let compose = self.compose.as_ref()?;
        if let Some(id) = compose.editing() {
            // The edited comment's box is already rendered in its column; scope
            // the editor to match it.
            let region = self.document.comments.iter().find(|r| r.id == id)?;
            return self.document.box_columns[region.header];
        }
        match compose.add_target()? {
            CommentTarget::Lines { side, .. } => Some(*side),
            // A reply threads beneath its parent, whose box already stands in a
            // column; scope the reply editor to that same column.
            CommentTarget::Comment { id } => {
                let region = self
                    .document
                    .comments
                    .iter()
                    .find(|r| r.id == BoxId::Comment(*id))?;
                self.document.box_columns[region.header]
            }
            _ => None,
        }
    }

    /// The floating editor drawn over the ordinary document while it is detached
    /// from its anchor: the box, the row it sits on, and the background to clear
    /// behind it. The box tracks the anchor's on-screen position, coming to rest
    /// at a screen edge once the anchor scrolls out of view. The cursor is
    /// reported only while it is in the editor, so a roaming cursor leaves the
    /// box without a hardware cursor. `None` when the editor is anchored inline
    /// or closed.
    pub fn compose_float(&self, width: usize) -> Option<FloatView> {
        let compose = self.compose.as_ref()?;
        let focus = self.detach?;
        if self.height == 0 {
            return None;
        }
        let max_interior = self.editor_max_interior();
        let wide = compose.layout(width.saturating_sub(2), max_interior);
        // Inset the box a couple of columns each side, giving the roaming
        // cursor's tint blank margin cells beside the border to read against.
        // The narrower text must wrap to the same height, else keep the wide box
        // so the inset never grows the box.
        const INSET: usize = 2;
        let inset_interior = width.saturating_sub(2 + 2 * INSET);
        let narrow = (inset_interior >= 1).then(|| compose.layout(inset_interior, max_interior));
        let (inset, editor) = match narrow {
            Some(narrow) if narrow.rows.len() == wide.rows.len() => (INSET as u16, narrow),
            _ => (0, wide),
        };
        let box_height = editor.rows.len() + 2;
        // Place the box where the anchor sits on screen, then clamp so it never
        // spills past the bottom: as the anchor scrolls toward an edge the box
        // follows it and then rests against that edge once it scrolls off.
        let anchor_row = self.compose_anchor().saturating_sub(self.top);
        let top_row = anchor_row.min(self.height.saturating_sub(box_height));
        let editor_cursor = match focus {
            DetachFocus::Inside => editor.cursor,
            DetachFocus::Outside => None,
        };
        // While the cursor roams the diff behind the box, its covered line cannot
        // show its own tint, so report the box row it sits on for the chrome to
        // show the tint around the editor's edges.
        let cursor_offset = match focus {
            DetachFocus::Outside if self.cursor >= self.top => {
                let screen = self.cursor - self.top;
                (top_row..top_row + box_height)
                    .contains(&screen)
                    .then(|| (screen - top_row) as u16)
            }
            _ => None,
        };
        Some(FloatView {
            top_row: top_row.min(u16::MAX as usize) as u16,
            background: self.background,
            editor_block: compose.block(&self.editor_hint()),
            editor_rows: editor.rows,
            editor_cursor,
            editor_scroll: editor.scroll,
            inset,
            cursor_bg: self.cursor_bg,
            cursor_offset,
        })
    }

    /// Begin leaving the review for a quit `action`: resolve it against the
    /// configured default and any pending drafts, either settling how to leave
    /// immediately or opening the picker to ask.
    fn request_exit(&mut self, action: Action) -> Update {
        match plan_exit(action, self.exit_default, self.has_drafts()) {
            ExitPlan::Now(exit) => self.exit = Some(exit),
            ExitPlan::Ask {
                title,
                choices,
                selected,
            } => {
                let rows: Vec<Box<dyn PickerRow<App>>> = choices
                    .into_iter()
                    .map(|(label, exit)| {
                        Box::new(ExitRow {
                            label: label.to_string(),
                            exit,
                        }) as Box<dyn PickerRow<App>>
                    })
                    .collect();
                let hint = self.picker_hint();
                let mut picker = Picker::new(title, rows, &hint, self.picker_colors);
                picker.select(selected);
                self.picker = Some(picker);
            }
        }
        Update::Handled
    }

    /// Whether the review has uncommitted draft edits that leaving would lose.
    fn has_drafts(&self) -> bool {
        self.review.as_ref().is_some_and(Review::has_drafts)
    }

    /// The chosen way to leave once a quit has resolved, which the host reads to
    /// end the loop. Absent until the reviewer settles the choice.
    pub fn pending_exit(&self) -> Option<Exit> {
        self.exit
    }

    /// Open the modal list of the diff's files, each jumping to that file's
    /// header when chosen. Does nothing when the diff has no files.
    fn open_file_picker(&mut self) {
        let rows: Vec<Box<dyn PickerRow<App>>> = self
            .document
            .files
            .iter()
            .enumerate()
            .filter_map(|(file, path)| {
                let row = self.document.rows.iter().position(|meta| {
                    meta.file == file && matches!(meta.kind, RowKind::FileHeader)
                })?;
                Some(Box::new(FileRow {
                    path: path.clone(),
                    row,
                }) as Box<dyn PickerRow<App>>)
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        let hint = self.picker_hint();
        self.picker = Some(Picker::new("Jump to file", rows, &hint, self.picker_colors));
    }

    /// Open the modal list of the diff's comments, each jumping to that comment
    /// when chosen. Does nothing when the diff carries no comments.
    fn open_comment_picker(&mut self) {
        // Pair each comment placed in the document with its effective state --
        // the committed comment with any buffered draft edit applied.
        let states: HashMap<Ulid, EffectiveComment> = match self.review.as_ref() {
            Some(review) => review
                .comment_states()
                .into_iter()
                .map(|entry| (entry.comment.id, entry))
                .collect(),
            None => return,
        };
        let mut listed: Vec<(usize, EffectiveComment)> = self
            .document
            .comments
            .iter()
            .enumerate()
            .filter_map(|(position, region)| {
                let BoxId::Comment(id) = region.id else {
                    return None;
                };
                states.get(&id).cloned().map(|entry| (position, entry))
            })
            .collect();
        if listed.is_empty() {
            return;
        }
        // Group by status -- draft, then open, resolved, and withdrawn -- keeping
        // each group in the order the comments appear in the document.
        listed.sort_by_key(|(position, entry)| {
            (comment_bucket(&entry.comment, entry.pending), *position)
        });

        let locations: Vec<String> = listed
            .iter()
            .map(|(_, entry)| comment_location(&entry.comment.target))
            .collect();
        let location_width = locations
            .iter()
            .map(|location| location.chars().count())
            .max()
            .unwrap_or(0);
        let author_width = listed
            .iter()
            .map(|(_, entry)| entry.comment.author.name.chars().count())
            .max()
            .unwrap_or(0);
        // A committed comment shows its review-scoped number; a draft has none
        // until committed and leaves the column blank.
        let numbers: Vec<String> = listed
            .iter()
            .map(|(_, entry)| {
                entry
                    .comment
                    .number
                    .map(|number| number.to_string())
                    .unwrap_or_default()
            })
            .collect();
        let number_width = numbers
            .iter()
            .map(|number| number.chars().count())
            .max()
            .unwrap_or(0);

        let rows: Vec<Box<dyn PickerRow<App>>> = listed
            .iter()
            .zip(&locations)
            .zip(&numbers)
            .map(|(((_, entry), location), number)| {
                let (marker, marker_color) = self.comment_marker(&entry.comment, entry.pending);
                let author = entry.comment.author.name.as_str();
                let preview = comment_preview(&entry.comment.body);
                // The number column and its separator vanish entirely when no
                // listed comment has a number, so an all-drafts picker does not
                // read as indented by a blank column.
                let number_column = if number_width == 0 {
                    String::new()
                } else {
                    format!("{number:<number_width$}  ")
                };
                let body = format!(
                    "{number_column}{location:<location_width$}  {author:<author_width$}  {preview}"
                );
                let withdrawn = entry.comment.deleted;
                let spans = vec![
                    RowSpan {
                        text: marker.to_string(),
                        color: marker_color,
                        strikethrough: false,
                    },
                    RowSpan {
                        text: body,
                        color: withdrawn.then_some(self.comment_marker_colors.muted),
                        strikethrough: withdrawn,
                    },
                ];
                Box::new(CommentRow {
                    spans,
                    id: entry.comment.id,
                }) as Box<dyn PickerRow<App>>
            })
            .collect();

        let hint = self.picker_hint();
        self.picker = Some(Picker::new(
            "Jump to comment",
            rows,
            &hint,
            self.picker_colors,
        ));
    }

    /// The two-column status marker for a comment and the color it is painted:
    /// `*` while it has an uncommitted change, a check once resolved, `!` when
    /// its anchor has drifted off its lines, and blank otherwise. A withdrawn
    /// comment shows no marker, reading as struck-through text instead.
    fn comment_marker(&self, comment: &CommentState, pending: bool) -> (&'static str, Option<Rgb>) {
        let colors = self.comment_marker_colors;
        if pending {
            ("* ", Some(colors.draft))
        } else if comment.deleted {
            ("  ", None)
        } else if comment.resolved {
            ("\u{2713} ", Some(colors.muted))
        } else if matches!(
            comment.confidence,
            Some(Confidence::Approximate | Confidence::Outdated)
        ) {
            ("! ", Some(colors.warn))
        } else {
            ("  ", None)
        }
    }

    /// Open the modal list of color themes, each recoloring the whole view when
    /// chosen. The list opens on the theme in effect. Does nothing on a
    /// read-only view with no review to recolor.
    fn open_theme_picker(&mut self) {
        if self.review.is_none() {
            return;
        }
        let names = wiff_diff::theme_names();
        let selected = names.iter().position(|name| *name == self.theme_name);
        let rows: Vec<Box<dyn PickerRow<App>>> = names
            .into_iter()
            .map(|name| Box::new(ThemeRow { name }) as Box<dyn PickerRow<App>>)
            .collect();
        if rows.is_empty() {
            return;
        }
        let hint = self.picker_hint();
        let mut picker = Picker::new("Theme", rows, &hint, self.picker_colors);
        if let Some(index) = selected {
            picker.select(index);
        }
        self.picker = Some(picker);
    }

    /// Open the modal list of captured versions to compare the review against.
    /// Reports instead when there is no earlier version, and does nothing on a
    /// read-only view.
    fn open_compare_picker(&mut self) {
        let Some(latest) = self.latest_version() else {
            return;
        };
        if latest == 0 {
            self.set_message("no earlier version to compare against".to_string());
            return;
        }
        self.picker =
            Some(self.compare_picker("Compare versions", latest, self.comparing_from(), None));
    }

    /// Build the modal list of captured versions to compare against, titled
    /// `title`. It offers the latest version's own diff and every earlier
    /// version as a reference point. `showing` is the version the reviewer sees
    /// as their current view (`None` for the latest diff): its row is marked and
    /// the list opens on it. When `last_commented` is given, that version's row
    /// is marked as where the reviewer last committed comments.
    fn compare_picker(
        &self,
        title: &str,
        latest: u32,
        showing: Option<u32>,
        last_commented: Option<u32>,
    ) -> Picker<App> {
        let annotate = |text: String, version: Option<u32>| {
            let mut marks = Vec::new();
            if showing == version {
                marks.push("showing now");
            }
            // Only an actual earlier version can be where comments were last
            // committed; the latest-diff row (a `None` version) never is.
            if let Some(last) = last_commented
                && version == Some(last)
            {
                marks.push("your last comments");
            }
            if marks.is_empty() {
                text
            } else {
                format!("{text} ({})", marks.join(", "))
            }
        };
        let mut choices = vec![(
            annotate(format!("the latest diff (v{latest})"), None),
            CompareRequest::Latest,
        )];
        for k in (0..latest).rev() {
            choices.push((
                annotate(format!("changes since v{k}"), Some(k)),
                CompareRequest::Version(k),
            ));
        }
        let selected = match showing {
            None => 0,
            Some(from) => choices
                .iter()
                .position(|(_, request)| *request == CompareRequest::Version(from))
                .unwrap_or(0),
        };
        let rows: Vec<Box<dyn PickerRow<App>>> = choices
            .into_iter()
            .map(|(label, request)| VersionRow::boxed(label, request))
            .collect();
        let hint = self.picker_hint();
        let mut picker = Picker::new(title, rows, &hint, self.picker_colors);
        picker.select(selected);
        picker
    }

    /// Take the version comparison the reviewer chose from the picker, if any,
    /// clearing it.
    pub fn take_pending_compare(&mut self) -> Option<CompareRequest> {
        self.pending_compare.take()
    }

    /// Open a modal asking whether to recapture the source, for a launch where
    /// the source has changed since the latest captured version. Choosing to
    /// refresh records the request for the host to act on. Does nothing on a
    /// read-only view with no review.
    pub fn offer_refresh(&mut self) {
        let Some(latest) = self.latest_version() else {
            return;
        };
        let rows: Vec<Box<dyn PickerRow<App>>> = vec![
            Box::new(RefreshRow {
                label: "Refresh now".to_string(),
                refresh: true,
            }) as Box<dyn PickerRow<App>>,
            Box::new(RefreshRow {
                label: "Keep the current diff".to_string(),
                refresh: false,
            }) as Box<dyn PickerRow<App>>,
        ];
        let hint = self.picker_hint();
        let title = format!("The source has changed since v{latest}");
        self.picker = Some(Picker::new(&title, rows, &hint, self.picker_colors));
    }

    /// Take whether the reviewer chose to refresh from the launch prompt,
    /// clearing it.
    pub fn take_pending_refresh(&mut self) -> bool {
        std::mem::take(&mut self.pending_refresh)
    }

    /// Open the compare-versions list after a refresh, keeping the reviewer's
    /// perspective from before the recapture: `showing` is the version they were
    /// viewing (`None` for the latest diff) and its row opens marked as where
    /// they were, and `last_commented` marks where they last committed comments.
    /// The list is the same one the compare hotkey opens, under a title naming
    /// the capture that prompted it. Cancelling keeps that same perspective
    /// against the fresh capture, still a valid view since the comparison's
    /// right side is always the latest. Does nothing when there is no earlier
    /// version or on a read-only view with no review.
    pub fn offer_compare_after_refresh(
        &mut self,
        showing: Option<u32>,
        last_commented: Option<u32>,
    ) {
        let Some(latest) = self.latest_version() else {
            return;
        };
        if latest == 0 {
            return;
        }
        let title = format!("Captured v{latest}: compare against a version, or keep the latest");
        self.picker = Some(self.compare_picker(&title, latest, showing, last_commented));
        self.compare_on_cancel = Some(match showing {
            Some(from) => CompareRequest::Version(from),
            None => CompareRequest::Latest,
        });
    }

    /// The picker's key hint, naming the reviewer's own bindings for moving the
    /// highlight alongside the fixed enter and escape keys that select and
    /// cancel, resolved from the active keymap like the inline editor's hint.
    fn picker_hint(&self) -> String {
        let up = self
            .keymap
            .primary_label(Action::LineUp)
            .unwrap_or_default();
        let down = self
            .keymap
            .primary_label(Action::LineDown)
            .unwrap_or_default();
        format!("{up}/{down} move  enter select  esc cancel")
    }

    /// Whether the modal list is open, so the host routes raw key presses to it
    /// rather than resolving them into actions.
    pub fn picking(&self) -> bool {
        self.picker.is_some()
    }

    /// The open modal list, for the host to render centered over the view.
    pub fn picker(&self) -> Option<&Picker<App>> {
        self.picker.as_ref()
    }

    /// Set how many rows the open modal list shows, from the space the host
    /// gives it. Does nothing when the list is closed.
    pub fn picker_set_height(&mut self, height: usize) {
        if let Some(picker) = self.picker.as_mut() {
            picker.set_height(height);
        }
    }

    /// Move the highlight in the open modal list with a resolved navigation
    /// action, so it moves with the reviewer's own movement bindings. Ignores
    /// any non-movement action, and does nothing when the list is closed.
    pub fn picker_nav(&mut self, action: Action) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match action {
            Action::LineDown => picker.select_next(),
            Action::LineUp => picker.select_prev(),
            Action::PageDown => picker.page_down(),
            Action::PageUp => picker.page_up(),
            Action::Top => picker.to_top(),
            Action::Bottom => picker.to_bottom(),
            _ => {}
        }
    }

    /// Close the modal list and act on its highlighted row. An explicit
    /// selection supersedes any cancel fallback, so that is dropped. Does
    /// nothing when the list is closed.
    pub fn picker_activate(&mut self) {
        self.compare_on_cancel = None;
        if let Some(picker) = self.picker.take() {
            picker.activate_selected(self);
        }
    }

    /// Close the modal list without acting on any row, recording the comparison
    /// the cancel resolves to when the picker set one.
    pub fn picker_cancel(&mut self) {
        if let Some(request) = self.compare_on_cancel.take() {
            self.pending_compare = Some(request);
        }
        self.picker = None;
    }

    /// Open the help overlay, listing the bindings active in the current keymap.
    fn open_help(&mut self) {
        self.help = Some(Help::new(&self.keymap, self.help_colors));
    }

    /// Whether the help overlay is open.
    pub fn helping(&self) -> bool {
        self.help.is_some()
    }

    /// The open help overlay, for the host to render centered over the view.
    pub fn help(&self) -> Option<&Help> {
        self.help.as_ref()
    }

    /// Set how many rows the open help overlay shows, from the space the host
    /// gives it. Does nothing when the overlay is closed.
    pub fn help_set_height(&mut self, height: usize) {
        if let Some(help) = self.help.as_mut() {
            help.set_height(height);
        }
    }

    /// Scroll the open help overlay with a resolved navigation action. Ignores
    /// any non-movement action, and does nothing when the overlay is closed.
    pub fn help_nav(&mut self, action: Action) {
        let Some(help) = self.help.as_mut() else {
            return;
        };
        match action {
            Action::LineDown => help.scroll_down(),
            Action::LineUp => help.scroll_up(),
            Action::PageDown => help.page_down(),
            Action::PageUp => help.page_up(),
            Action::Top => help.to_top(),
            Action::Bottom => help.to_bottom(),
            _ => {}
        }
    }

    /// Close the help overlay.
    pub fn close_help(&mut self) {
        self.help = None;
    }

    /// The background the whole view fills with, for the host to paint behind the
    /// document so the theme reads over the terminal's own background.
    pub fn background(&self) -> Rgb {
        self.background
    }

    /// Recolor the whole view to `theme`: re-cache the palette the app paints
    /// its own chrome with, re-theme the review so its document re-renders with
    /// the new syntax and diff colors, and reflow, keeping the fold and comment
    /// collapse state and the cursor in view. Leaves the view unchanged on an
    /// unknown syntax theme.
    pub fn apply_theme(&mut self, theme: &Theme) {
        if let Some(review) = self.review.as_mut()
            && review.set_theme(theme.clone()).is_err()
        {
            return;
        }
        self.theme_name = theme.syntax_theme.clone();
        self.background = theme.background;
        self.cursor_bg = theme.cursor_bg;
        self.search_match_bg = theme.search_match_bg;
        self.status_fg = theme.status_fg;
        self.status_bg = theme.status_bg;
        self.divider_fg = theme.gutter_fg;
        self.compose_border = theme.comment_draft_fg;
        self.picker_colors = PickerColors {
            border: theme.review_fg,
            background: theme.background,
            selected_bg: theme.cursor_bg,
            text: theme.comment_fg,
            hint: theme.fold_fg,
        };
        self.help_colors = HelpColors {
            border: theme.review_fg,
            background: theme.background,
            keys: theme.comment_author_fg,
            text: theme.comment_fg,
            hint: theme.fold_fg,
        };
        self.comment_marker_colors = CommentMarkerColors {
            draft: theme.comment_draft_fg,
            muted: theme.comment_flag_fg,
            warn: theme.comment_warn_fg,
        };
        if let Some(review) = self.review.as_ref() {
            let document = review.document(self.layout());
            self.reload_document(document);
        }
    }

    /// Whether any background parse is still in progress.
    pub fn highlighting(&self) -> bool {
        self.review.as_ref().is_some_and(Review::highlighting)
    }

    /// Fold any arrived background parse results into the review and re-render.
    /// Returns true when a newly colored file is on screen.
    pub fn poll_highlights(&mut self) -> bool {
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        let changed = review.poll_highlights();
        if changed.is_empty() {
            return false;
        }
        self.rerender();
        self.any_file_visible(&changed)
    }

    /// Whether any of `files` has a row within the visible window.
    fn any_file_visible(&self, files: &[usize]) -> bool {
        let end = (self.top + self.height).min(self.view.len());
        self.view[self.top..end].iter().any(|row| match row {
            ViewRow::Row(row) => files.contains(&self.document.rows[*row].file),
            ViewRow::Fold(_) => false,
        })
    }

    /// Re-render the document from the review after a buffered edit, preserving
    /// the fold and comment collapse state and keeping the cursor in view.
    fn rerender(&mut self) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let document = review.document(self.layout());
        self.reload_document(document);
    }

    /// Swap in a freshly rendered `document`, keeping the collapse state, and
    /// clamp the cursor into the rebuilt view.
    fn reload_document(&mut self, document: Document) {
        self.adopt_document(document);
        self.cursor = self.cursor.min(self.last_view());
        self.scroll_into_view();
    }

    /// Recapture the review over `diff` as version `version`, replacing the diff
    /// and its committed `comments` and rebasing pending drafts forward. Folds
    /// reset to collapsed since the diff's structure has moved, comment collapse
    /// state survives by identity, and the cursor returns to the same file and
    /// line it was on, or the nearest surviving line in that file. Passes through
    /// silently when no review is attached. Drafted line comments move through
    /// `old_diff`, which yields the diff a draft was authored against.
    pub fn refresh(
        &mut self,
        diff: wiff_diff::Diff,
        comments: Vec<wiff_core::review::CommentState>,
        version: u32,
        old_diff: impl FnMut(u32) -> wiff_core::Result<wiff_diff::Diff>,
    ) -> wiff_core::Result<()> {
        if self.review.is_none() {
            return Ok(());
        }
        let spot = self.cursor_spot();
        let layout = self.layout();
        let document = {
            let review = self.review.as_mut().expect("review present");
            review.refresh(diff, comments, version, old_diff)?;
            review.document(layout)
        };
        self.collapsed = vec![true; document.folds.len()];
        self.reconcile_comment_collapse(&document);
        self.document = document;
        self.rebuild_view();
        self.restore_spot(spot);
        Ok(())
    }

    /// Present `diff` in place of the current one, placing comments by
    /// `comparison` (the reference version and each file's before origin) so the
    /// review shows the changes since that version. Passing `None` returns to
    /// the latest version's own diff. Folds reset to collapsed since the diff's
    /// structure has moved, comment collapse state survives by identity, and the
    /// cursor returns to the same file and line, or the nearest surviving line.
    /// Passes through silently when no review is attached.
    pub fn show_comparison(
        &mut self,
        diff: Diff,
        comparison: Option<(u32, HashMap<String, LineOrigin>)>,
    ) {
        if self.review.is_none() {
            return;
        }
        let spot = self.cursor_spot();
        let layout = self.layout();
        let document = {
            let review = self.review.as_mut().expect("review present");
            review.show_diff(diff, comparison);
            review.document(layout)
        };
        self.collapsed = vec![true; document.folds.len()];
        self.reconcile_comment_collapse(&document);
        self.document = document;
        self.rebuild_view();
        self.restore_spot(spot);
    }

    /// The reference version the review is comparing against, or `None` when it
    /// shows the latest version's own diff.
    pub fn comparing_from(&self) -> Option<u32> {
        self.review.as_ref().and_then(Review::comparing_from)
    }

    /// The latest captured version under review, or `None` on a read-only view
    /// with no review attached.
    fn latest_version(&self) -> Option<u32> {
        self.review.as_ref().map(Review::version)
    }

    /// Carry each comment's collapse state onto a freshly rendered `document`,
    /// matching by identity, so a re-render keeps what the reviewer had folded
    /// and gives a comment new to the document its rendered default.
    fn reconcile_comment_collapse(&mut self, document: &Document) {
        self.comment_collapsed = document
            .comments
            .iter()
            .map(|region| {
                let collapsed = self
                    .comment_collapsed
                    .get(&region.id)
                    .copied()
                    .unwrap_or(region.collapsed_default);
                (region.id, collapsed)
            })
            .collect();
    }

    /// Replace the review's committed comments with `comments`, the set the host
    /// returns after persisting the drafts, and re-render in place. The cursor
    /// returns to its spot. Reports how the reloaded set differs from what was
    /// shown, so the host can tell the reviewer what another actor changed.
    /// Passes through silently when no review is attached.
    pub fn reload_comments(
        &mut self,
        comments: Vec<wiff_core::review::CommentState>,
        description: Option<wiff_core::review::DescriptionState>,
    ) -> CommentSync {
        if self.review.is_none() {
            return CommentSync::default();
        }
        let spot = self.cursor_spot();
        let layout = self.layout();
        let (sync, document) = {
            let review = self.review.as_mut().expect("review present");
            let sync = review.set_committed(comments, description);
            (sync, review.document(layout))
        };
        self.adopt_document(document);
        self.restore_spot(spot);
        sync
    }

    /// Show `message` in the status line until the reviewer's next action, used
    /// to report the outcome of a refresh.
    pub fn set_message(&mut self, message: String) {
        self.message = Some(message);
    }

    /// Whether the search prompt is open, so the host routes raw key presses to
    /// it rather than resolving them into actions.
    pub fn searching(&self) -> bool {
        self.search.is_some()
    }

    /// Feed a key press to the open search prompt: extend or edit the pattern and
    /// jump to the first match as it changes, accept the pattern, or abandon the
    /// prompt and return to where it opened. Does nothing when it is closed.
    pub fn search_key(&mut self, press: KeyPress) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        match search.key(press) {
            SearchInput::Edited => {
                let pattern = search.pattern().to_string();
                let direction = search.direction();
                let origin = search.origin();
                match self.find_match(&pattern, direction, origin, true) {
                    Some(row) => self.reveal_and_focus(row),
                    None => self.focus_doc_row(origin),
                }
            }
            SearchInput::Submit => {
                let pattern = search.pattern().to_string();
                let direction = search.direction();
                self.search = None;
                if !pattern.is_empty() {
                    self.active_search = Some((pattern.clone(), direction));
                    self.last_search = Some((pattern, direction));
                }
            }
            SearchInput::Cancel => {
                let origin = search.origin();
                self.search = None;
                self.focus_doc_row(origin);
            }
            SearchInput::Ignored => {}
        }
    }

    /// Open the search prompt scanning `direction`, anchored to the row the
    /// cursor is on so an abandoned or empty search returns there.
    fn start_search(&mut self, direction: Direction) {
        self.search = Some(Search::new(direction, self.cursor_doc_row()));
    }

    /// The left of the status bar while a search is live. While typing it is the
    /// prompt with its lead character and the live match tally; once `accepted`
    /// it also names the keys that repeat the search in each direction, so the
    /// reviewer sees the term, how to step through the matches, and where the
    /// cursor sits among them.
    fn search_bar(&self, pattern: &str, direction: Direction, accepted: bool) -> String {
        let lead = direction.lead();
        if pattern.is_empty() {
            return lead.to_string();
        }
        let tally = self.match_tally(pattern);
        if accepted {
            let next = self
                .keymap
                .primary_label(Action::SearchNext)
                .unwrap_or_default();
            let prev = self
                .keymap
                .primary_label(Action::SearchPrev)
                .unwrap_or_default();
            let mut bar = format!("{lead}{pattern}  {next} next  {prev} prev  {tally}");
            if let Some(note) = &self.search_note {
                bar.push_str("  ");
                bar.push_str(note);
            }
            bar
        } else {
            format!("{lead}{pattern}  {tally}")
        }
    }

    /// The document rows matching `pattern` in document order, skipping rows
    /// hidden inside a collapsed fold, so the tally counts the same matches the
    /// search steps through.
    fn match_rows(&self, matcher: &Matcher) -> Vec<usize> {
        (0..self.document.rows.len())
            .filter(|&row| !self.row_in_collapsed_fold(row))
            .filter(|&row| matcher.is_match(&self.document.text[row]))
            .collect()
    }

    /// The `X/Y matches` tally for `pattern`, where `X` is the match the cursor
    /// is on, a plain note when nothing matches, or a note that the pattern is
    /// not yet a valid regex. The pattern is never empty here, so a failure to
    /// compile means the reviewer is mid-way through typing an expression.
    fn match_tally(&self, pattern: &str) -> String {
        let Some(matcher) = Matcher::new(pattern) else {
            return "bad pattern".to_string();
        };
        let rows = self.match_rows(&matcher);
        if rows.is_empty() {
            return "no matches".to_string();
        }
        let current = self.cursor_doc_row();
        let index = rows
            .iter()
            .position(|&row| row == current)
            .map_or(0, |i| i + 1);
        format!("{index}/{} matches", rows.len())
    }

    /// Repeat the last accepted search from the cursor, in its own direction or
    /// reversed. Reports a wrap around the ends of the document, or that nothing
    /// matched, in the status line.
    fn repeat_search(&mut self, reverse: bool) {
        let Some((pattern, direction)) = self.last_search.clone() else {
            self.message = Some("no previous search".to_string());
            return;
        };
        let scan = if reverse {
            direction.reversed()
        } else {
            direction
        };
        let from = self.cursor_doc_row();
        match self.find_match(&pattern, scan, from, false) {
            Some(row) => {
                let wrapped = match scan {
                    Direction::Forward => row <= from,
                    Direction::Backward => row >= from,
                };
                self.reveal_and_focus(row);
                if wrapped {
                    self.search_note = Some(
                        match scan {
                            Direction::Forward => "wrapped to top",
                            Direction::Backward => "wrapped to bottom",
                        }
                        .to_string(),
                    );
                }
                self.active_search = Some((pattern, direction));
            }
            None => self.message = Some(format!("pattern not found: {pattern}")),
        }
    }

    /// The document row the cursor sits on, resolving a fold marker to the first
    /// row it hides.
    fn cursor_doc_row(&self) -> usize {
        match self.view.get(self.cursor) {
            Some(ViewRow::Row(row)) => *row,
            Some(ViewRow::Fold(fold)) => self.document.folds[*fold].start,
            None => 0,
        }
    }

    /// Whether document `row` is hidden inside a currently collapsed fold, so a
    /// search passes over it.
    fn row_in_collapsed_fold(&self, row: usize) -> bool {
        self.fold_containing(row)
            .is_some_and(|fold| self.collapsed[fold])
    }

    /// The first document row matching `pattern` scanning `direction` from row
    /// `from`, wrapping around the ends. `include_from` searches `from` itself
    /// first, for an incremental search that may already sit on a match; a
    /// repeat leaves it out so it moves off the current match. Rows hidden in a
    /// collapsed fold never match; a collapsed comment's body still does.
    fn find_match(
        &self,
        pattern: &str,
        direction: Direction,
        from: usize,
        include_from: bool,
    ) -> Option<usize> {
        let matcher = Matcher::new(pattern)?;
        let n = self.document.rows.len();
        if n == 0 {
            return None;
        }
        for step in 0..n {
            let offset = if include_from { step } else { step + 1 };
            let row = match direction {
                Direction::Forward => (from + offset) % n,
                Direction::Backward => (from + 2 * n - offset) % n,
            };
            if self.row_in_collapsed_fold(row) {
                continue;
            }
            if matcher.is_match(&self.document.text[row]) {
                return Some(row);
            }
        }
        None
    }

    /// Move the cursor onto document `row`, expanding its comment first when the
    /// match sits in a collapsed comment body so the matched line is shown.
    fn reveal_and_focus(&mut self, row: usize) {
        if let RowKind::CommentBody { id } = self.document.rows[row].kind
            && self.is_comment_collapsed(id)
        {
            self.comment_collapsed.insert(id, false);
            self.rebuild_view();
        }
        self.focus_doc_row(row);
    }

    /// Move the cursor onto document `row`, or onto the marker of the collapsed
    /// fold that hides it, keeping it within the viewport.
    fn focus_doc_row(&mut self, row: usize) {
        let index = self.view_index_of_row(row).or_else(|| {
            self.fold_containing(row)
                .and_then(|fold| self.view_index_of_fold(fold))
        });
        if let Some(index) = index {
            self.cursor = index;
            self.scroll_into_view();
        }
    }

    /// Where the cursor is, to return to after the document is rebuilt. Absent
    /// when the cursor is on no file.
    fn cursor_spot(&self) -> Option<CursorSpot> {
        let file = self.document.files.get(self.cursor_file()?)?.clone();
        let place = match self.kind_at(self.cursor) {
            Some(kind) if kind.content_addr().is_some() => {
                let (side, lineno) = kind.content_addr()?;
                SpotPlace::Line(side, lineno)
            }
            Some(
                RowKind::CommentHeader { id }
                | RowKind::CommentBody { id }
                | RowKind::CommentBottom { id },
            ) => SpotPlace::Comment(*id),
            _ => SpotPlace::Header,
        };
        Some(CursorSpot { file, place })
    }

    /// Return the cursor to `spot` after a rebuild, else the top of the document
    /// when neither it nor its file survived.
    fn restore_spot(&mut self, spot: Option<CursorSpot>) {
        let target = spot
            .and_then(|spot| self.spot_row(&spot))
            .and_then(|row| self.locate_document_row(row));
        self.move_to(target.unwrap_or(0));
    }

    /// The document row that best returns the cursor to `spot`: a surviving
    /// comment by its header wherever it now renders, a content line exactly or
    /// by its nearest surviving neighbor, else the file's header. `None` when the
    /// file itself is gone.
    fn spot_row(&self, spot: &CursorSpot) -> Option<usize> {
        if let SpotPlace::Comment(id) = spot.place
            && let Some(row) = self.comment_header_row(id)
        {
            return Some(row);
        }
        let file = self.document.files.iter().position(|f| *f == spot.file)?;
        let line = match spot.place {
            SpotPlace::Line(side, lineno) => Some((side, lineno)),
            _ => None,
        };
        self.best_row_in_file(file, line)
    }

    /// The best document row to land on within file `file` for a cursor that was
    /// on `line`: the exact line if present, else the nearest line on the same
    /// side, else the file's header row.
    fn best_row_in_file(&self, file: usize, line: Option<(Side, LineNo)>) -> Option<usize> {
        if let Some((side, target)) = line {
            let mut nearest: Option<(usize, u32)> = None;
            for (row, meta) in self.document.rows.iter().enumerate() {
                if meta.file != file {
                    continue;
                }
                let Some((row_side, lineno)) = meta.kind.content_addr() else {
                    continue;
                };
                if row_side != side {
                    continue;
                }
                let distance = lineno.get().abs_diff(target.get());
                if distance == 0 {
                    return Some(row);
                }
                if nearest.is_none_or(|(_, best)| distance < best) {
                    nearest = Some((row, distance));
                }
            }
            if let Some((row, _)) = nearest {
                return Some(row);
            }
        }
        self.document
            .rows
            .iter()
            .position(|meta| meta.file == file && matches!(meta.kind, RowKind::FileHeader))
    }

    /// The view index showing document `row`, or the marker of the fold that
    /// hides it when it is folded away.
    fn locate_document_row(&self, row: usize) -> Option<usize> {
        self.view_index_of_row(row).or_else(|| {
            self.fold_containing(row)
                .and_then(|fold| self.view_index_of_fold(fold))
        })
    }

    /// Move the cursor to comment `id`'s header row, if it is in view.
    fn focus_comment(&mut self, id: BoxId) {
        if let Some(index) = self
            .comment_header_row(id)
            .and_then(|row| self.view_index_of_row(row))
        {
            self.move_to(index);
        }
    }

    /// The lines currently in view, drawn out to the full `width`: comment rows
    /// as box edges, diff rows tinted to the edge by role, and the cursor row
    /// washed in the selection color on top of whichever of those it is.
    pub fn visible(&self, width: usize) -> Vec<Line<'static>> {
        let end = (self.top + self.height).min(self.view.len());
        let matcher = self.highlight_pattern().and_then(Matcher::new);
        let compose_span = self.compose_rail_span();
        (self.top..end)
            .map(|i| {
                let mut line = self.decorate(i, width, compose_span);
                // The cursor row, and every row of a live selection, wash in the
                // cursor color so the marked span reads as one block.
                if i == self.cursor || self.row_selected(i) {
                    line = wash(line, self.cursor_bg, width, self.background);
                }
                if let (Some(matcher), ViewRow::Row(row)) = (&matcher, &self.view[i]) {
                    line = self.highlight_matches(line, *row, matcher, width);
                }
                line
            })
            .collect()
    }

    /// The pattern whose matches the view highlights: the one being typed while
    /// the prompt is open, otherwise the accepted search still shown in the
    /// status bar, or none once the search is left behind.
    fn highlight_pattern(&self) -> Option<&str> {
        if let Some(search) = &self.search {
            let pattern = search.pattern();
            (!pattern.is_empty()).then_some(pattern)
        } else {
            self.active_search
                .as_ref()
                .map(|(pattern, _)| pattern.as_str())
        }
    }

    /// Wash the search-match background over each occurrence of `matcher` in the
    /// already-decorated `line` for document `row`, so every visible match reads
    /// as highlighted on top of whatever tint the row and cursor gave it. A
    /// side-by-side content row washes each column against its own text, since
    /// the two runs are separated by a gutter and the divider rule.
    fn highlight_matches(
        &self,
        line: Line<'static>,
        row: usize,
        matcher: &Matcher,
        width: usize,
    ) -> Line<'static> {
        if let Some(split) = self.document.row_columns[row].as_ref() {
            // Split at the divider column, not on the glyph, since an anchor rail
            // in a gutter draws the same glyph as the divider.
            let (left, rest) = split_line_at(line, divider_column(width));
            let (divider, right) = split_line_at(rest, 1);
            let left = self.wash_run(left, &split.left, matcher);
            let right = self.wash_run(right, &split.right, matcher);
            let mut spans = left.spans;
            spans.extend(divider.spans);
            spans.extend(right.spans);
            return Line::from(spans);
        }
        self.wash_run(line, &self.document.text[row], matcher)
    }

    /// Wash the search-match background over each occurrence of `matcher` in
    /// `line`, whose content contains `text` as one contiguous run past any
    /// gutter, header prefix, or box edge; locating that run once maps each
    /// match's offset within `text` to the line. `text` is assumed to appear in
    /// the line once; a second occurrence in the surrounding chrome would shift
    /// the mapping.
    fn wash_run(&self, line: Line<'static>, text: &str, matcher: &Matcher) -> Line<'static> {
        let ranges = matcher.ranges(text);
        if ranges.is_empty() {
            return line;
        }
        let content: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let Some(base) = content.find(text) else {
            return line;
        };
        let ranges: Vec<Range<usize>> = ranges
            .into_iter()
            .map(|range| base + range.start..base + range.end)
            .collect();
        highlight_spans(line, &ranges, self.search_match_bg, self.background)
    }

    /// The fully drawn line for view row `index` at `width`, before any cursor
    /// wash: a comment's box edges, a diff row's role tint filled to the edge, or
    /// the fold marker on the plain background.
    fn decorate(
        &self,
        index: usize,
        width: usize,
        compose_span: Option<(usize, usize, Side)>,
    ) -> Line<'static> {
        match self.view[index] {
            ViewRow::Fold(fold) => {
                let marker = self.document.folds[fold].marker.clone();
                fill_line(marker, self.document.folds[fold].fill, width)
            }
            ViewRow::Row(row) => {
                let line = self.document.lines[row].clone();
                let fill = self.document.fills[row];
                match self.document.rows[row].kind {
                    RowKind::CommentHeader { .. } => match self.document.box_columns[row] {
                        Some(side) => {
                            let (_, w) = column_bounds(width, side);
                            // Clip the title to the box interior; a column is too
                            // narrow for the full author-and-hints line, and an
                            // overflow would push the divider out of alignment.
                            let title = split_line_at(line, w.saturating_sub(4)).0;
                            self.place_in_column(box_top(title, fill, w), width, side)
                        }
                        None => box_top(line, fill, width),
                    },
                    RowKind::CommentBody { .. } => match self.document.box_columns[row] {
                        Some(side) => {
                            let (_, w) = column_bounds(width, side);
                            self.place_in_column(box_side(line, fill, w), width, side)
                        }
                        None => box_side(line, fill, width),
                    },
                    RowKind::CommentBottom { id } => {
                        let anchors = self.box_anchors_rail(id);
                        match self.document.box_columns[row] {
                            Some(side) => {
                                let (_, w) = column_bounds(width, side);
                                let tee = anchors.then_some(COLUMN_GUTTER_WIDTH - 1);
                                self.place_in_column(box_bottom(fill, w, tee), width, side)
                            }
                            None => box_bottom(fill, width, anchors.then_some(RAIL_COLUMN)),
                        }
                    }
                    RowKind::Content { .. } => {
                        let mut line = fill_line(line, fill, width);
                        // The rail is comment chrome, so it shows only while
                        // comments do and is dropped when they are hidden.
                        if !self.comments_hidden {
                            for cell in self.row_rails(row, compose_span) {
                                line = overlay_rail(line, cell, self.background);
                            }
                        }
                        line
                    }
                    _ => fill_line(line, fill, width),
                }
            }
        }
    }

    /// Place a column-scoped comment box line at its `side` column of a viewport
    /// `width` columns wide, filling the opposite column with blanks and drawing
    /// the divider rule between the two. `box_line` is the box edge already drawn
    /// at the column's width.
    fn place_in_column(&self, box_line: Line<'static>, width: usize, side: Side) -> Line<'static> {
        let divider = Span::styled(
            COLUMN_DIVIDER.to_string(),
            Style::default().fg(color(self.divider_fg)),
        );
        let left_width = divider_column(width);
        let right_width = width.saturating_sub(left_width + 1);
        match side {
            Side::Before => {
                let mut spans = box_line.spans;
                spans.push(divider);
                spans.push(Span::raw(" ".repeat(right_width)));
                Line::from(spans)
            }
            Side::After => {
                let mut spans = vec![Span::raw(" ".repeat(left_width)), divider];
                spans.extend(box_line.spans);
                Line::from(spans)
            }
        }
    }

    /// The diff-row kind at view row `index`, or `None` when it is a fold marker
    /// (which the landmark jumps skip over).
    fn kind_at(&self, index: usize) -> Option<&RowKind> {
        match self.view[index] {
            ViewRow::Row(row) => Some(&self.document.rows[row].kind),
            ViewRow::Fold(_) => None,
        }
    }

    /// Rebuild the view from the collapse state: each collapsed fold becomes one
    /// marker in place of the rows it hides, the body rows of a collapsed comment
    /// drop out behind its header, and every other row appears in order. When
    /// comments are hidden, every comment row drops out too, leaving the code.
    fn rebuild_view(&mut self) {
        let hidden = self.hidden_comment_rows();
        self.view.clear();
        let mut row = 0;
        while row < self.document.rows.len() {
            match self.fold_starting_at(row) {
                Some(fold) if self.collapsed[fold] => {
                    self.view.push(ViewRow::Fold(fold));
                    row = self.document.folds[fold].end;
                }
                _ => {
                    let dropped = hidden[row] || (self.comments_hidden && self.is_comment_row(row));
                    if !dropped {
                        self.view.push(ViewRow::Row(row));
                    }
                    row += 1;
                }
            }
        }
    }

    /// Whether the comment `id` anchors a line range, so its box bottom drops the
    /// anchor rail into the gutter.
    fn box_anchors_rail(&self, id: BoxId) -> bool {
        self.document
            .comments
            .iter()
            .any(|region| region.id == id && region.anchor_rail)
    }

    /// The anchor rails document `row` draws: the ones baked in by the render,
    /// plus, while a range comment is being authored, a preview of the rail its
    /// target will keep, so the reviewer sees the covered span before saving.
    /// `compose_span` is the covered span from [`compose_rail_span`], passed in
    /// so a full render resolves it once rather than for every drawn row.
    fn row_rails(&self, row: usize, compose_span: Option<(usize, usize, Side)>) -> Vec<RailCell> {
        let mut rails = self.document.rails[row].clone();
        if let Some(cell) = self.compose_rail(row, compose_span) {
            rails.retain(|c| c.column != cell.column);
            rails.push(cell);
        }
        rails
    }

    /// The inclusive document-row span the in-progress range comment previews its
    /// rail over, and the side it anchors to. `None` when not authoring a range
    /// comment or when its lines are not currently rendered. Resolved once per
    /// render and handed to [`row_rails`] so the per-row preview does not rescan
    /// the document.
    fn compose_rail_span(&self) -> Option<(usize, usize, Side)> {
        let CommentTarget::Lines {
            file,
            side,
            start_line,
            end_line,
        } = self.compose.as_ref()?.add_target()?
        else {
            return None;
        };
        let start_row = self.first_content_row(file, *side, start_line.get())?;
        let end_row = self.first_content_row(file, *side, end_line.get())?;
        Some((start_row, end_row, *side))
    }

    /// The preview rail cell for document `row` within `compose_span`, the covered
    /// span of the in-progress range comment: the draft-colored body glyph down
    /// the span, closing with the corner on its last row. Every content row in
    /// the span draws it, including the opposite-side rows a unified diff
    /// interleaves, so the preview reads as one unbroken stroke. `None` outside
    /// the span or on a non-content row.
    fn compose_rail(
        &self,
        row: usize,
        compose_span: Option<(usize, usize, Side)>,
    ) -> Option<RailCell> {
        let (start_row, end_row, side) = compose_span?;
        if row < start_row || row > end_row {
            return None;
        }
        if !matches!(self.document.rows[row].kind, RowKind::Content { .. }) {
            return None;
        }
        // The corner closes the rail on the first display row of the end line;
        // its wrapped continuations sit past `end_row` and stay blank.
        let glyph = if row == end_row {
            '\u{2514}'
        } else {
            '\u{2502}'
        };
        Some(RailCell {
            // The render width places the column the baked document rails sit
            // in, so the preview aligns with them.
            column: rail_column(self.document.mode, side, self.width),
            glyph,
            color: self.compose_border,
        })
    }

    /// The first display row of the content line `(file, side, lineno)` addresses,
    /// or `None` when no rendered line matches.
    fn first_content_row(&self, file: &str, side: Side, lineno: u32) -> Option<usize> {
        self.document.rows.iter().position(|row| {
            let Some((row_side, n)) = row.kind.content_addr() else {
                return false;
            };
            row_side == side
                && n.get() == lineno
                && self.document.files.get(row.file).map(String::as_str) == Some(file)
        })
    }

    /// Whether document `row` is part of a comment box: its header, a body line,
    /// or its bottom edge.
    fn is_comment_row(&self, row: usize) -> bool {
        matches!(
            self.document.rows[row].kind,
            RowKind::CommentHeader { .. }
                | RowKind::CommentBody { .. }
                | RowKind::CommentBottom { .. }
        )
    }

    /// Which document rows are the body of a currently-collapsed comment, and so
    /// are hidden behind their header row.
    fn hidden_comment_rows(&self) -> Vec<bool> {
        let mut hidden = vec![false; self.document.rows.len()];
        for region in &self.document.comments {
            if self.is_comment_collapsed(region.id) {
                for row in region.body.clone() {
                    hidden[row] = true;
                }
            }
        }
        hidden
    }

    /// Whether the comment `id` is currently collapsed. A comment absent from the
    /// map has never been toggled, so it keeps its rendered default.
    fn is_comment_collapsed(&self, id: BoxId) -> bool {
        self.comment_collapsed.get(&id).copied().unwrap_or(false)
    }

    /// The fold that begins at document `row`, if any.
    fn fold_starting_at(&self, row: usize) -> Option<usize> {
        self.document
            .folds
            .iter()
            .position(|fold| fold.start == row)
    }

    /// The fold whose hidden range covers document `row`, if any.
    fn fold_containing(&self, row: usize) -> Option<usize> {
        self.document
            .folds
            .iter()
            .position(|fold| fold.start <= row && row < fold.end)
    }

    /// The view index showing document `row`, if it is currently visible.
    fn view_index_of_row(&self, row: usize) -> Option<usize> {
        self.view
            .iter()
            .position(|entry| matches!(entry, ViewRow::Row(r) if *r == row))
    }

    /// The view index of `fold`'s marker, if it is currently collapsed.
    fn view_index_of_fold(&self, fold: usize) -> Option<usize> {
        self.view
            .iter()
            .position(|entry| matches!(entry, ViewRow::Fold(f) if *f == fold))
    }

    /// Expand the fold under the cursor, or collapse the fold the cursor sits
    /// inside, landing the cursor on the revealed content or the new marker.
    fn toggle_fold(&mut self) {
        match self.view[self.cursor] {
            ViewRow::Fold(fold) => {
                self.collapsed[fold] = false;
                let first = self.document.folds[fold].start;
                self.rebuild_view();
                if let Some(index) = self.view_index_of_row(first) {
                    self.move_to(index);
                }
            }
            ViewRow::Row(row) => {
                if let Some(fold) = self.fold_containing(row) {
                    self.collapsed[fold] = true;
                    self.rebuild_view();
                    if let Some(index) = self.view_index_of_fold(fold) {
                        self.move_to(index);
                    }
                }
            }
        }
    }

    /// Expand or collapse the comment the cursor is on, whether it sits on the
    /// header or somewhere in the body, landing the cursor back on the header.
    fn toggle_comment(&mut self) {
        let Some(id) = self.comment_at_cursor() else {
            return;
        };
        let collapsed = self.is_comment_collapsed(id);
        self.comment_collapsed.insert(id, !collapsed);
        self.rebuild_view();
        self.focus_comment(id);
    }

    /// Show or hide every comment. Hiding drops the annotations from the view so
    /// the code reads uncrowded; the cursor keeps its place on the code, moving
    /// to the anchored line when it sat on a comment that is now gone.
    fn toggle_comments_hidden(&mut self) {
        let anchor = self.cursor_doc_row();
        self.comments_hidden = !self.comments_hidden;
        self.rebuild_view();
        self.focus_nearest_visible_row(anchor);
    }

    /// Move the cursor onto document `row`, or the nearest following row still in
    /// view when `row` itself has dropped out, falling back to the last row.
    fn focus_nearest_visible_row(&mut self, from: usize) {
        let target = (from..self.document.rows.len())
            .find(|&row| self.view_index_of_row(row).is_some() || self.row_in_collapsed_fold(row));
        match target {
            Some(row) => self.focus_doc_row(row),
            None => self.move_to(self.last_view()),
        }
    }

    /// The comment the cursor is on, whether on the box's top edge, a body line,
    /// or the bottom edge.
    fn comment_at_cursor(&self) -> Option<BoxId> {
        match self.kind_at(self.cursor)? {
            RowKind::CommentHeader { id }
            | RowKind::CommentBody { id }
            | RowKind::CommentBottom { id } => Some(*id),
            _ => None,
        }
    }

    /// The count of consecutive view rows starting at `anchor` that render
    /// comment `id`: its header, its body when expanded, and its bottom edge.
    fn comment_rows(&self, anchor: usize, id: BoxId) -> usize {
        let mut count = 0;
        while anchor + count < self.view.len() {
            match self.kind_at(anchor + count) {
                Some(
                    RowKind::CommentHeader { id: row_id }
                    | RowKind::CommentBody { id: row_id }
                    | RowKind::CommentBottom { id: row_id },
                ) if *row_id == id => count += 1,
                _ => break,
            }
        }
        count
    }

    /// The first view row at or after `from` that is not part of a comment box,
    /// so a newly authored comment's editor lands after the comments already
    /// placed there, where its own rendered box will appear once saved.
    fn skip_comment_rows(&self, from: usize) -> usize {
        let mut index = from;
        while index < self.view.len()
            && matches!(
                self.kind_at(index),
                Some(
                    RowKind::CommentHeader { .. }
                        | RowKind::CommentBody { .. }
                        | RowKind::CommentBottom { .. }
                )
            )
        {
            index += 1;
        }
        index
    }

    /// The document row of comment `id`'s header line.
    fn comment_header_row(&self, id: BoxId) -> Option<usize> {
        self.document
            .comments
            .iter()
            .find(|region| region.id == id)
            .map(|region| region.header)
    }

    /// A page's worth of rows for page up/down, at least one.
    fn page(&self) -> usize {
        self.height.max(1)
    }

    /// The last addressable view row, or zero for an empty view.
    fn last_view(&self) -> usize {
        self.view.len().saturating_sub(1)
    }

    /// The furthest the viewport can scroll while still filling the screen.
    fn max_top(&self) -> usize {
        self.view.len().saturating_sub(self.height)
    }

    /// Move the cursor to `target`, clamped to the view, then scroll just enough
    /// to keep it visible.
    fn move_to(&mut self, target: usize) {
        self.cursor = target.min(self.last_view());
        self.scroll_into_view();
    }

    /// Advance a whole page: scroll the viewport down by a screen and carry the
    /// cursor with it, as `less` does on space.
    fn page_down(&mut self) {
        let step = self.page();
        self.top = (self.top + step).min(self.max_top());
        self.cursor = (self.cursor + step).min(self.last_view());
        self.clamp_cursor_visible();
    }

    /// Retreat a whole page: scroll the viewport up by a screen and carry the
    /// cursor with it.
    fn page_up(&mut self) {
        let step = self.page();
        self.top = self.top.saturating_sub(step);
        self.cursor = self.cursor.saturating_sub(step);
        self.clamp_cursor_visible();
    }

    /// Move the cursor to the next `landmark` row after it, if any.
    fn jump_forward(&mut self, landmark: Landmark) {
        if let Some(index) = (self.cursor + 1..self.view.len())
            .find(|&i| self.kind_at(i).is_some_and(|kind| landmark.matches(kind)))
        {
            self.move_to(index);
        }
    }

    /// Move the cursor to the nearest `landmark` row before it, if any.
    fn jump_backward(&mut self, landmark: Landmark) {
        if let Some(index) = (0..self.cursor)
            .rev()
            .find(|&i| self.kind_at(i).is_some_and(|kind| landmark.matches(kind)))
        {
            self.move_to(index);
        }
    }

    /// Slide the viewport so the cursor row stays visible with a scrolloff
    /// margin of rows above and below it, as far as the ends of the view allow.
    fn scroll_into_view(&mut self) {
        if self.height == 0 {
            return;
        }
        // A third of the viewport is kept between the cursor and either edge so a
        // single-line move on a tall screen still scrolls the view rather than
        // walking the cursor a long way to the edge first. Half the height less
        // one is the most a centered cursor leaves room for.
        let margin = (self.height / 3).min((self.height - 1) / 2);
        let above = self.cursor.saturating_sub(margin);
        if self.top > above {
            self.top = above;
        }
        let below = (self.cursor + margin + 1).saturating_sub(self.height);
        if self.top < below {
            self.top = below;
        }
        self.top = self.top.min(self.max_top());
    }

    /// The status line for the bottom of the screen: the file the cursor is in
    /// on the left and how far through the view it sits right-aligned, filled to
    /// `width`.
    pub fn status(&self, width: usize) -> Line<'static> {
        let percent = format!("{}%", self.progress_percent());
        let text: String = if let Some(search) = &self.search {
            let left = self.search_bar(search.pattern(), search.direction(), false);
            status_row(&left, &percent, width)
        } else if let Some((pattern, direction)) = &self.active_search {
            let left = self.search_bar(pattern, *direction, true);
            status_row(&left, &percent, width)
        } else if let Some(message) = &self.message {
            message.chars().take(width).collect()
        } else {
            let path = self
                .cursor_file()
                .and_then(|file| self.document.files.get(file))
                .map(String::as_str)
                .unwrap_or("");
            status_row(path, &self.status_meta(&percent), width)
        };
        Line::from(Span::styled(
            format!("{text:<width$}"),
            Style::default()
                .fg(color(self.status_fg))
                .bg(color(self.status_bg))
                .add_modifier(Modifier::BOLD),
        ))
    }

    /// The right-aligned status segment for the file view: a `*` when
    /// uncommitted drafts are buffered, the number of open comments (or a note
    /// naming the key that shows them again while they are hidden), and
    /// `percent`, how far the cursor sits through the view.
    fn status_meta(&self, percent: &str) -> String {
        let marker = if self.has_drafts() { "* " } else { "" };
        let compare = match (self.comparing_from(), self.latest_version()) {
            (Some(from), Some(latest)) => format!("v{from}..v{latest}  "),
            _ => String::new(),
        };
        if self.comments_hidden {
            let key = self
                .keymap
                .primary_label(Action::HideComments)
                .unwrap_or_default();
            return format!("{marker}{compare}comments hidden, toggle with {key}  {percent}");
        }
        let open = self.review.as_ref().map_or(0, Review::open_comments);
        format!("{marker}{compare}{open} open  {percent}")
    }

    /// The file index the cursor is in: its own row's file, or the file of the
    /// first row hidden by the fold the cursor is on.
    fn cursor_file(&self) -> Option<usize> {
        let row = match self.view.get(self.cursor)? {
            ViewRow::Row(row) => *row,
            ViewRow::Fold(fold) => self.document.folds[*fold].start,
        };
        self.document.rows.get(row).map(|row| row.file)
    }

    /// How far the cursor sits through the view, from zero at the top to a
    /// hundred at the last row.
    fn progress_percent(&self) -> usize {
        match self.last_view() {
            0 => 100,
            last => self.cursor * 100 / last,
        }
    }

    /// Pull the cursor back into the viewport after a page scroll clamped the
    /// top, so it never sits off the visible rows.
    fn clamp_cursor_visible(&mut self) {
        if self.cursor < self.top {
            self.cursor = self.top;
        } else if self.height > 0 && self.cursor >= self.top + self.height {
            self.cursor = self.top + self.height - 1;
        }
    }
}

/// The direction a plain arrow steps, used to nudge the editor loose when the
/// cursor is already at its edge. Only the arrows count: the page keys page
/// within the body even from the edge row, so treating them as a nudge would
/// switch modality out from under a reviewer paging through their comment.
/// `None` for any other press.
fn nudge_direction(press: &KeyPress) -> Option<Nudge> {
    if press.ctrl || press.alt || press.shift {
        return None;
    }
    match press.key {
        Key::Up => Some(Nudge::Up),
        Key::Down => Some(Nudge::Down),
        _ => None,
    }
}

/// Whether `action` moves or searches the diff without mutating a comment or
/// opening a modal, so it is safe to run while the editor floats detached and
/// the cursor roams.
fn is_navigation(action: Action) -> bool {
    matches!(
        action,
        Action::LineDown
            | Action::LineUp
            | Action::PageDown
            | Action::PageUp
            | Action::Top
            | Action::Bottom
            | Action::NextFile
            | Action::PrevFile
            | Action::NextHunk
            | Action::PrevHunk
            | Action::NextComment
            | Action::PrevComment
            | Action::ToggleFold
            | Action::ToggleWrap
            | Action::DiffModeAuto
            | Action::DiffModeSideBySide
            | Action::DiffModeUnified
            | Action::DiffModeOnlyAfter
            | Action::HideComments
            | Action::SearchForward
            | Action::SearchBackward
            | Action::SearchNext
            | Action::SearchPrev
    )
}

/// Lay out the status line: `left` at the start and `right` flush against the
/// end at `width`, spaces filling the gap between them. When they cannot both
/// fit, `right` is kept whole and `left` is truncated to make room.
fn status_row(left: &str, right: &str, width: usize) -> String {
    let right: String = right.chars().take(width).collect();
    let room = width - right.chars().count();
    let left: String = left.chars().take(room).collect();
    let gap = room - left.chars().count();
    format!("{left}{:gap$}{right}", "")
}

/// The style for a comment box's border in `color`, or an unstyled border when
/// no color is set.
fn border_style(border: Option<Rgb>) -> Style {
    match border {
        Some(rgb) => Style::default().fg(color(rgb)),
        None => Style::default(),
    }
}

/// The number of columns the spans of `line` occupy.
fn line_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

/// Draw the top edge of a comment box out to `width`: the corner, the row's
/// spans as the title, then a rule to the closing corner at the last column.
fn box_top(title: Line<'static>, border: Option<Rgb>, width: usize) -> Line<'static> {
    let style = border_style(border);
    let mut spans = vec![Span::styled("\u{250c} ", style)];
    let mut used = 2;
    for span in title.spans {
        used += span.content.chars().count();
        spans.push(span);
    }
    spans.push(Span::styled(" ", style));
    used += 1;
    if width > used + 1 {
        spans.push(Span::styled("\u{2500}".repeat(width - used - 1), style));
    }
    spans.push(Span::styled("\u{2510}", style));
    Line::from(spans)
}

/// Draw a body row of a comment box out to `width`: the left edge, the content,
/// blank cells to the last column, then the right edge.
fn box_side(content: Line<'static>, border: Option<Rgb>, width: usize) -> Line<'static> {
    let style = border_style(border);
    let used = 1 + line_width(&content);
    let mut spans = vec![Span::styled("\u{2502}", style)];
    spans.extend(content.spans);
    if width > used + 1 {
        spans.push(Span::styled(" ".repeat(width - used - 1), Style::default()));
    }
    spans.push(Span::styled("\u{2502}", style));
    Line::from(spans)
}

/// Draw the bottom edge of a comment box out to `width`: the corners joined by a
/// rule. When `tee` names a column within the box's own `width` the box anchors
/// a line range, so the rule drops a tee there to join the anchor rail tracing
/// the lines below.
fn box_bottom(border: Option<Rgb>, width: usize, tee: Option<usize>) -> Line<'static> {
    let style = border_style(border);
    let mut chars = vec!['\u{2514}'];
    if width > 2 {
        chars.extend(std::iter::repeat_n('\u{2500}', width - 2));
    }
    chars.push('\u{2518}');
    if let Some(col) = tee.filter(|&c| c > 0 && c + 1 < width) {
        chars[col] = RAIL_TEE;
    }
    Line::from(Span::styled(chars.into_iter().collect::<String>(), style))
}

/// Overlay one anchor rail on a content line: swap the single cell at
/// `cell.column` for its glyph in the rail color, lifted to read over the tint
/// of the gutter it sits in, leaving every other cell untouched. A column past
/// the line's end leaves the line unchanged. Each span is expected to have an
/// explicit background, the tint the glyph must stay legible over; a span with
/// none takes the rail color unadjusted.
fn overlay_rail(line: Line<'static>, cell: RailCell, base: Rgb) -> Line<'static> {
    let mut out = Vec::with_capacity(line.spans.len() + 2);
    let mut offset = 0;
    let mut placed = false;
    for span in line.spans {
        let count = span.content.chars().count();
        if placed || cell.column < offset || cell.column >= offset + count {
            offset += count;
            out.push(span);
            continue;
        }
        let cut = cell.column - offset;
        let chars: Vec<char> = span.content.chars().collect();
        let before: String = chars[..cut].iter().collect();
        let after: String = chars[cut + 1..].iter().collect();
        let fg = match rgb_of(span.style.bg) {
            Some(bg) => legible_over(cell.color, bg, base),
            None => cell.color,
        };
        out.push(Span::styled(before, span.style));
        out.push(Span::styled(
            cell.glyph.to_string(),
            span.style.fg(color(fg)),
        ));
        out.push(Span::styled(after, span.style));
        offset += count;
        placed = true;
    }
    Line::from(out)
}

/// Split `line` at character column `at` into the cells before it and the cells
/// from it on, dividing the span that straddles the column and keeping every
/// style. A column past the line's end returns the whole line and an empty tail.
fn split_line_at(line: Line<'static>, at: usize) -> (Line<'static>, Line<'static>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut offset = 0;
    for span in line.spans {
        let count = span.content.chars().count();
        if offset >= at {
            right.push(span);
        } else if offset + count <= at {
            offset += count;
            left.push(span);
        } else {
            let cut = at - offset;
            let chars: Vec<char> = span.content.chars().collect();
            left.push(Span::styled(
                chars[..cut].iter().collect::<String>(),
                span.style,
            ));
            right.push(Span::styled(
                chars[cut..].iter().collect::<String>(),
                span.style,
            ));
            offset += count;
        }
    }
    (Line::from(left), Line::from(right))
}

/// The RGB behind a span background, or `None` when it is unset or a non-RGB
/// terminal color, so the rail lift only fires over a known tint.
fn rgb_of(bg: Option<Color>) -> Option<Rgb> {
    match bg {
        Some(Color::Rgb(r, g, b)) => Some(Rgb { r, g, b }),
        _ => None,
    }
}

/// Return `line` padded with blank cells in `fill` out to `width`, so a row that
/// already tints its content extends that tint to the edge of the screen. A row
/// with no fill is left untouched.
fn fill_line(mut line: Line<'static>, fill: Option<Rgb>, width: usize) -> Line<'static> {
    let Some(bg) = fill else {
        return line;
    };
    let filled: usize = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    if width > filled {
        line.spans.push(Span::styled(
            " ".repeat(width - filled),
            Style::default().bg(color(bg)),
        ));
    }
    line
}

/// Return `line` with `bg` washed over the byte `ranges` of its concatenated
/// content, splitting the spans they cut across so only the matched glyphs take
/// the background while every span keeps its foreground and modifiers. Walking
/// character by character keeps the split safe whatever the ranges hold: a byte
/// offset never has to fall on a boundary for the code to be correct.
fn highlight_spans(
    line: Line<'static>,
    ranges: &[Range<usize>],
    bg: Rgb,
    reference: Rgb,
) -> Line<'static> {
    let mut out = Vec::with_capacity(line.spans.len());
    let mut offset = 0;
    for span in line.spans {
        let base = offset;
        let content = span.content.into_owned();
        offset += content.len();
        // Gather runs of characters that share a covered state, so a match
        // landing inside the span takes the highlight while the rest keeps the
        // span's own style.
        let mut piece = String::new();
        let mut covered = false;
        for (index, ch) in content.char_indices() {
            let here = ranges.iter().any(|range| range.contains(&(base + index)));
            if !piece.is_empty() && here != covered {
                out.push(highlighted_piece(
                    &piece, span.style, covered, bg, reference,
                ));
                piece.clear();
            }
            covered = here;
            piece.push(ch);
        }
        if !piece.is_empty() {
            out.push(highlighted_piece(
                &piece, span.style, covered, bg, reference,
            ));
        }
    }
    Line::from(out)
}

/// One highlighted or plain piece of a split span: `style` with the match
/// background laid over it when `covered`, otherwise `style` untouched. A
/// covered piece keeps its foreground legible over the match background, lifted
/// to the contrast it had over `reference` when the match color would dim it.
fn highlighted_piece(
    text: &str,
    style: Style,
    covered: bool,
    bg: Rgb,
    reference: Rgb,
) -> Span<'static> {
    let style = if covered {
        legible_style(style, bg, reference).bg(color(bg))
    } else {
        style
    };
    Span::styled(text.to_string(), style)
}

/// Return `style` with its foreground lifted to stay legible over `bg`, keeping
/// the contrast it had over `reference`. A style with no explicit `Rgb`
/// foreground is left untouched, since there is nothing to measure.
fn legible_style(style: Style, bg: Rgb, reference: Rgb) -> Style {
    match style.fg {
        Some(Color::Rgb(r, g, b)) => {
            let adjusted = legible_over(Rgb { r, g, b }, bg, reference);
            style.fg(color(adjusted))
        }
        _ => style,
    }
}

/// Return `line` with every span's background replaced by `bg`, keeping each
/// span's foreground and modifiers, then padded with blank cells to `width` so
/// the background fills the row to the edge of the screen.
fn wash(mut line: Line<'static>, bg: Rgb, width: usize, reference: Rgb) -> Line<'static> {
    for span in &mut line.spans {
        span.style = legible_style(span.style, bg, reference).bg(color(bg));
    }
    let filled: usize = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    if width > filled {
        line.spans.push(Span::styled(
            " ".repeat(width - filled),
            Style::default().bg(color(bg)),
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use ulid::Ulid;
    use wiff_core::record::{
        Author, AuthorKind, CommentEvent, CommentEventKind, CommentNumber, CommentTarget,
        Confidence, Description, DescriptionRecord, Disposition, RecordBody, Seq, VersionNumber,
    };
    use wiff_core::review::{CommentState, DescriptionState};
    use wiff_diff::{Diff, FileStatus, LineKind, Side};

    use super::{App, CompareRequest, ComposeView, FloatView, Update};
    use crate::action::Action;
    use crate::exit::{Exit, ExitDefault};
    use crate::key::{Chord, Key, KeyPress};
    use crate::keymap::{Keymap, KeymapOverrides};
    use crate::render::testutil::{dump, file, ln, theme};
    use crate::render::{BoxId, DiffMode, DiffView, RowKind, ViewLayout};
    use crate::review::Review;
    use crate::theme::Theme;

    /// A press of the printable character `c`.
    fn ch(c: char) -> KeyPress {
        KeyPress::new(Key::Char(c))
    }

    /// Feed each character of `text` to the open editor as a key press.
    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.compose_key(ch(c));
        }
    }

    /// The submit chord for the editor.
    fn submit() -> KeyPress {
        KeyPress::with_modifiers(Key::Char('d'), true, false, false)
    }

    /// A review over a two-line Rust file with no comments yet, for authoring
    /// tests.
    fn plain_review() -> Review {
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
        Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            Vec::new(),
            None,
        )
    }

    /// The editor body and the document lines above and below it, as a human
    /// sees them stacked down the screen, with the cursor's column,row within
    /// the editor named on the editor divider.
    fn dump_compose(view: &ComposeView) -> String {
        let cursor = match view.editor_cursor {
            Some((col, row)) => format!("cursor {col},{row}"),
            None => "cursor off".to_string(),
        };
        format!(
            "{}--editor {cursor}--\n{}--below--\n{}",
            dump(&view.above),
            dump(&view.editor_rows),
            dump(&view.below),
        )
    }

    /// The screen as a human sees it while the editor floats detached: the diff
    /// rows with the floating editor box overlaid on the row it tracks, and the
    /// tracked row and cursor named on the box's divider. The box covers the
    /// diff rows it sits over, matching the real overlay in [`crate::run`].
    fn dump_detached(app: &App, width: usize, height: usize) -> String {
        let mut doc = app.visible(width);
        // The document area is `height` rows tall; a short diff leaves blank
        // rows the floating box may sit over, so pad to the full height first.
        doc.resize(height, ratatui::text::Line::from(String::new()));
        let float = app.compose_float(width).expect("detached");
        let cursor = match float.editor_cursor {
            Some((col, row)) => format!("cursor {col},{row}"),
            None => "cursor off".to_string(),
        };
        let top_row = float.top_row as usize;
        let box_end = (top_row + float.editor_rows.len() + 2).min(doc.len());
        format!(
            "{}--float row {top_row} {cursor}--\n{}--float end--\n{}",
            dump(&doc[..top_row.min(doc.len())]),
            dump(&float.editor_rows),
            dump(&doc[box_end..]),
        )
    }

    /// The document row the floating editor's top border sits on, or `None` when
    /// the editor is anchored inline or closed.
    fn float_top(app: &App) -> Option<usize> {
        app.compose_float(TEST_WIDTH)
            .map(|view| view.top_row as usize)
    }

    /// The blank margin the floating box holds on each side, or `None` when the
    /// editor is anchored inline or closed.
    fn float_inset(app: &App) -> Option<u16> {
        app.compose_float(TEST_WIDTH).map(|view| view.inset)
    }

    /// Whether the floating editor shows a cursor, i.e. focus is inside it.
    fn float_has_cursor(app: &App) -> bool {
        app.compose_float(TEST_WIDTH)
            .and_then(|view: FloatView| view.editor_cursor)
            .is_some()
    }

    /// The box row the roaming diff cursor sits behind, or `None` when it is
    /// clear of the box, in the editor, or the editor is inline or closed.
    fn float_cursor_offset(app: &App) -> Option<u16> {
        app.compose_float(TEST_WIDTH)
            .and_then(|view| view.cursor_offset)
    }

    /// An after-side line comment on `line` of `path` by `author`, with `body`,
    /// resolved when `resolved`.
    fn line_comment(
        id: u128,
        author: (&str, AuthorKind),
        path: &str,
        line: u32,
        body: &str,
        resolved: bool,
    ) -> CommentState {
        CommentState {
            id: Ulid(id),
            author: Author {
                name: author.0.to_string(),
                kind: author.1,
            },
            target: CommentTarget::Lines {
                file: path.to_string(),
                side: Side::After,
                start_line: ln(line),
                end_line: ln(line),
            },
            version: VersionNumber(0),
            anchor: None,
            body: body.to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: Author {
                name: author.0.to_string(),
                kind: author.1,
            },
            resolved,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            disposition: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            // This hand-built state feeds rendering directly, so the number is
            // whatever the snapshot asserts, not a fold's output. Fixtures pass
            // ids in create order, so reusing the id as the number reads
            // naturally.
            number: Some(CommentNumber(id as u32)),
            created_seq: Seq(0),
            updated_seq: Seq(0),
        }
    }

    /// A committed reply by `author` answering comment `root`, sequenced at
    /// `seq` so it orders within its thread by log order.
    fn reply_comment(
        id: u128,
        author: (&str, AuthorKind),
        root: u128,
        body: &str,
        seq: u64,
    ) -> CommentState {
        CommentState {
            target: CommentTarget::Comment { id: Ulid(root) },
            created_seq: Seq(seq),
            updated_seq: Seq(seq),
            ..line_comment(id, author, "src/lib.rs", 2, body, false)
        }
    }

    /// A one-file diff whose second line carries an unresolved two-line comment
    /// and whose first line carries a resolved one.
    fn commented_diff() -> (Diff, Vec<CommentState>) {
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
        let comments = vec![
            line_comment(1, ("opus", AuthorKind::Agent), "src/lib.rs", 1, "ok", true),
            line_comment(
                2,
                ("wez", AuthorKind::Human),
                "src/lib.rs",
                2,
                "why 2?\nsay more",
                false,
            ),
        ];
        (diff, comments)
    }

    /// The [`commented_diff`] rendered as a static document, with no review
    /// attached, for navigation and collapse tests.
    fn commented_document() -> crate::render::Document {
        let (diff, comments) = commented_diff();
        DiffView::new(theme())
            .unwrap()
            .render_review(&diff, &comments, &[], ViewLayout::default())
    }

    /// The [`commented_diff`] as an editable review, for comment-authoring tests.
    fn commented_review() -> Review {
        let (diff, comments) = commented_diff();
        Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            comments,
            None,
        )
    }

    /// A two-file document: a Rust modification and a short text edit.
    fn document() -> crate::render::Document {
        let diff = Diff {
            files: vec![
                file(
                    "src/lib.rs",
                    FileStatus::Modified,
                    &[
                        (LineKind::Context, "let x = 1;", 1),
                        (LineKind::Added, "let y = 2;", 2),
                    ],
                ),
                file(
                    "notes.txt",
                    FileStatus::Added,
                    &[(LineKind::Added, "hello", 1)],
                ),
            ],
        };
        DiffView::new(theme()).unwrap().render(&diff)
    }

    /// A single text file whose one change is buried in long runs of unchanged
    /// context, so the leading and trailing runs fold away.
    fn folded_document() -> crate::render::Document {
        let mut lines: Vec<(LineKind, String, u32)> = Vec::new();
        for n in 1..=8 {
            lines.push((LineKind::Context, format!("ctx{n:02}"), n));
        }
        lines.push((LineKind::Added, "change!".to_string(), 9));
        for n in 9..=16 {
            lines.push((LineKind::Context, format!("ctx{n:02}"), n + 1));
        }
        let borrowed: Vec<(LineKind, &str, u32)> =
            lines.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect();
        let diff = Diff {
            files: vec![file("notes.txt", FileStatus::Modified, &borrowed)],
        };
        DiffView::new(theme()).unwrap().render(&diff)
    }

    /// A single added file long enough that the cursor scrolls with a margin
    /// well before reaching the bottom of a tall viewport.
    fn tall_document() -> crate::render::Document {
        let lines: Vec<(LineKind, String, u32)> = (1..=20)
            .map(|n| (LineKind::Added, format!("row{n:02}"), n))
            .collect();
        let borrowed: Vec<(LineKind, &str, u32)> =
            lines.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect();
        let diff = Diff {
            files: vec![file("long.txt", FileStatus::Added, &borrowed)],
        };
        DiffView::new(theme()).unwrap().render(&diff)
    }

    /// The width the test viewport renders at, wide enough that the cursor row
    /// pads past every line's content so the full-width highlight shows.
    const TEST_WIDTH: usize = 40;

    /// Drive `actions` through a fresh app over `document` and return its cursor,
    /// top, and the dumped visible lines.
    fn drive(
        document: crate::render::Document,
        height: usize,
        actions: &[Action],
    ) -> (usize, usize, String) {
        let mut app = App::new(document, height, &theme());
        for action in actions {
            app.update(*action);
        }
        (app.cursor(), app.top(), dump(&app.visible(TEST_WIDTH)))
    }

    /// Drive `actions` over the two-file [`document`].
    fn after(height: usize, actions: &[Action]) -> (usize, usize, String) {
        drive(document(), height, actions)
    }

    /// Drive `actions` through a fresh app editing `review` and return its
    /// cursor, top, and the dumped visible lines.
    fn drive_review(review: Review, height: usize, actions: &[Action]) -> (usize, usize, String) {
        let mut app = App::reviewing(review, height, &theme());
        for action in actions {
            app.update(*action);
        }
        (app.cursor(), app.top(), dump(&app.visible(TEST_WIDTH)))
    }

    #[test]
    fn opens_with_the_cursor_on_the_first_file_header() {
        let (cursor, top, visible) = after(3, &[]);
        wince::assert_eq!(cursor, 0);
        wince::assert_eq!(top, 0);
        // The first three rows are shown; the cursor row (the file header) is
        // washed with the selection background out to the full width.
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#f6f6f8|#65737e|b>modified  src/lib.rs<-|#65737e|->                    \n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
        );
    }

    #[test]
    fn setting_the_height_centers_the_initial_cursor() {
        // Applying a height of nine (as the first draw does) drops the cursor
        // onto the middle visible row with the view still anchored at the top.
        let mut app = App::new(tall_document(), 0, &theme());
        app.set_height(9);
        wince::assert_eq!(app.cursor(), 4);
        wince::assert_eq!(app.top(), 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#c0c5ce|-|b>added  long.txt\n",
            "<#96b5b4|-|->@@ -1,20 +1,20 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->row01<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->        2 + <#c0c5ce|#414a4a|->row02<-|#414a4a|->                       \n",
            "<#f5f6f6|#65737e|->        3 + <#f6f6f8|#65737e|->row03<-|#65737e|->                       \n",
            "<#9ea1a9|#414a4a|->        4 + <#c0c5ce|#414a4a|->row04<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->        5 + <#c0c5ce|#414a4a|->row05<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->        6 + <#c0c5ce|#414a4a|->row06<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->        7 + <#c0c5ce|#414a4a|->row07<-|#414a4a|->                       \n",
        );
    }

    #[test]
    fn line_down_keeps_a_third_of_the_viewport_below_the_cursor() {
        // Twelve line-downs on a height-9 view: a third of nine is three, so the
        // cursor scrolls to sit three rows above the bottom rather than pinned to
        // the last visible line.
        let actions = [Action::LineDown; 12];
        let (cursor, top, visible) = drive(tall_document(), 9, &actions);
        wince::assert_eq!(cursor, 12);
        wince::assert_eq!(top, 7);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#9ea1a9|#414a4a|->        6 + <#c0c5ce|#414a4a|->row06<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->        7 + <#c0c5ce|#414a4a|->row07<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->        8 + <#c0c5ce|#414a4a|->row08<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->        9 + <#c0c5ce|#414a4a|->row09<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->       10 + <#c0c5ce|#414a4a|->row10<-|#414a4a|->                       \n",
            "<#f5f6f6|#65737e|->       11 + <#f6f6f8|#65737e|->row11<-|#65737e|->                       \n",
            "<#9ea1a9|#414a4a|->       12 + <#c0c5ce|#414a4a|->row12<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->       13 + <#c0c5ce|#414a4a|->row13<-|#414a4a|->                       \n",
            "<#9ea1a9|#414a4a|->       14 + <#c0c5ce|#414a4a|->row14<-|#414a4a|->                       \n",
        );
    }

    #[test]
    fn the_status_line_names_the_cursor_file_and_progress() {
        // At the top the first file is named and progress is zero; jumping to
        // the second file names it and shows how far through the view it sits.
        let mut app = App::new(document(), 10, &theme());
        wince::snapshot_display!(
            dump(&[app.status(28)]),
            "<#cdd1d8|#4f5b66|b>src/lib.rs        0 open  0%\n"
        );
        app.update(Action::NextFile);
        wince::snapshot_display!(
            dump(&[app.status(28)]),
            "<#cdd1d8|#4f5b66|b>notes.txt        0 open  66%\n"
        );
    }

    #[test]
    fn the_status_line_counts_open_comments_and_marks_uncommitted_edits() {
        // The review holds one open comment and one resolved; the status counts
        // the open one and shows no dirty marker. Reopening the resolved comment
        // buffers a draft, so the count rises to two and a `*` marks the
        // uncommitted edit.
        let mut app = App::reviewing(commented_review(), 12, &theme());
        for _ in 0..3 {
            app.update(Action::LineDown);
        }
        wince::snapshot_display!(
            dump(&[app.status(28)]),
            "<#cdd1d8|#4f5b66|b>src/lib.rs       1 open  33%\n"
        );
        app.update(Action::ResolveComment);
        wince::snapshot_display!(
            dump(&[app.status(28)]),
            "<#cdd1d8|#4f5b66|b>src/lib.rs     * 2 open  33%\n"
        );
    }

    #[test]
    fn a_verdict_cycles_only_on_a_comment_you_authored() {
        // The reviewer authored comment 2 but not comment 1, so a verdict
        // cycles through none, approve, and request changes on comment 2 while
        // comment 1 stays untouched.
        let mut review = commented_review();
        let dispositions = |review: &Review| -> Vec<Option<Disposition>> {
            review
                .comment_states()
                .iter()
                .map(|entry| entry.comment.disposition)
                .collect()
        };
        wince::assert_eq!(review.cycle_disposition(Ulid(1)), false);
        wince::assert_eq!(dispositions(&review), vec![None, None]);
        wince::assert_eq!(review.cycle_disposition(Ulid(2)), true);
        wince::assert_eq!(
            dispositions(&review),
            vec![None, Some(Disposition::Approve)]
        );
        review.cycle_disposition(Ulid(2));
        wince::assert_eq!(
            dispositions(&review),
            vec![None, Some(Disposition::RequestChanges)]
        );
        review.cycle_disposition(Ulid(2));
        wince::assert_eq!(dispositions(&review), vec![None, None]);
    }

    #[test]
    fn hiding_comments_replaces_the_open_count_with_the_toggle_key() {
        // With comments hidden the open count gives way to a note naming the key
        // that shows them again; showing them brings the count back.
        let mut app = App::reviewing(commented_review(), 12, &theme());
        for _ in 0..3 {
            app.update(Action::LineDown);
        }
        app.update(Action::HideComments);
        wince::snapshot_display!(
            dump(&[app.status(60)]),
            "<#cdd1d8|#4f5b66|b>src/lib.rs               comments hidden, toggle with H  75%\n"
        );
        app.update(Action::HideComments);
        wince::snapshot_display!(
            dump(&[app.status(60)]),
            "<#cdd1d8|#4f5b66|b>src/lib.rs                                       1 open  55%\n"
        );
    }

    #[test]
    fn a_long_comment_body_wraps_to_the_box_width_once_a_width_is_set() {
        // A single-line comment far wider than the view. Before a width is known
        // it renders on one row; setting the viewport width reflows it so the
        // body wraps to fit inside the box.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Added, "let y = 2;", 1)],
            )],
        };
        let review = Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            vec![line_comment(
                1,
                ("opus", AuthorKind::Agent),
                "src/lib.rs",
                1,
                "this comment runs well past the width of the box and must wrap",
                false,
            )],
            None,
        );
        let mut app = App::reviewing(review, 12, &theme());
        app.set_width(TEST_WIDTH);
        let visible = dump(&app.visible(TEST_WIDTH));
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#fcf7ee|#65737e|b>Review<#f7f7f8|#65737e|-> [press c here to draft the review comment]<#f7f7f8|#65737e|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->this comment runs well past the width<-|-|-> <#767b84|-|->│\n",
            "<#767b84|-|->│<#c0c5ce|-|->of the box and must wrap<-|-|->              <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        1 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn toggle_wrap_reflows_diff_content_to_the_viewport_and_back() {
        // A single added line wider than the content column. Clipped by default,
        // it stays one row that the draw truncates; toggling wrap reflows it
        // across rows broken at spaces, the gutter shown only on the first;
        // toggling again clips it back to one row.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(
                    LineKind::Added,
                    "let total = alpha plus beta plus gamma;",
                    1,
                )],
            )],
        };
        let review = Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            Vec::new(),
            None,
        );
        let mut app = App::reviewing(review, 12, &theme());
        app.set_width(TEST_WIDTH);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#fcf7ee|#65737e|b>Review<#f7f7f8|#65737e|-> [press c here to draft the review comment]<#f7f7f8|#65737e|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> total <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> alpha plus beta plus gamma<#c0c5ce|#414a4a|->;\n",
        );

        app.update(Action::ToggleWrap);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#fcf7ee|#65737e|b>Review<#f7f7f8|#65737e|-> [press c here to draft the review comment]<#f7f7f8|#65737e|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> total = alpha plus beta<-|#414a4a|-> \n",
            "<#9ea1a9|#414a4a|->            <#c0c5ce|#414a4a|->plus gamma;<-|#414a4a|->                 \n",
        );

        app.update(Action::ToggleWrap);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#fcf7ee|#65737e|b>Review<#f7f7f8|#65737e|-> [press c here to draft the review comment]<#f7f7f8|#65737e|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> total <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> alpha plus beta plus gamma<#c0c5ce|#414a4a|->;\n",
        );
    }

    #[test]
    fn a_comment_on_a_wrapped_continuation_row_anchors_above_the_lines_first_row() {
        // With wrap on, a long added line spans two rows. Adding a comment from
        // the second (continuation) row opens the editor above the line's first
        // row, where the submitted comment lands, not the continuation the
        // cursor sits on.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(
                    LineKind::Added,
                    "let total = alpha plus beta plus gamma;",
                    1,
                )],
            )],
        };
        let review = Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            Vec::new(),
            None,
        );
        let mut app = App::reviewing(review, 8, &theme());
        app.set_width(TEST_WIDTH);
        app.update(Action::ToggleWrap);
        // Land on the continuation row, the second of the wrapped line's rows.
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "why?");
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_compose(&view),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "--editor cursor 4,0--\n",
            "<#c0c5ce|-|->why?\n",
            "--below--\n",
            "<#9ea1a9|#414a4a|->        1 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> total = alpha plus beta<-|#414a4a|-> \n",
            "<#9ea1a9|#414a4a|->            <#c0c5ce|#414a4a|->plus gamma;<-|#414a4a|->                 \n",
        );
    }

    #[test]
    fn next_and_prev_file_jump_between_file_headers() {
        let (cursor, top, _) = after(10, &[Action::NextFile]);
        wince::assert_eq!(cursor, 4);
        wince::assert_eq!(top, 0);

        // From the second file, prev-file returns to the first header, and a
        // further prev-file stays put since there is none before it.
        let (cursor, top, _) = after(10, &[Action::NextFile, Action::PrevFile, Action::PrevFile]);
        wince::assert_eq!(cursor, 0);
        wince::assert_eq!(top, 0);
    }

    /// Dump the open modal list rendered to its full height, as a human sees it.
    fn dump_picker(app: &mut App) -> String {
        let rows = app.picker().expect("picking").list_len();
        app.picker_set_height(rows);
        dump(
            &app.picker()
                .expect("picking")
                .lines(app.picker().expect("picking").width()),
        )
    }

    #[test]
    fn the_file_picker_lists_every_file_with_the_first_highlighted() {
        // Opening the picker over the two-file diff lists both paths, the first
        // highlighted, then a spacer and the key hint.
        let mut app = App::new(document(), 8, &theme());
        app.update(Action::PickFile);
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_picker(&mut app),
            "<#c0c5ce|#65737e|->> src/lib.rs                            \n",
            "<#c0c5ce|#2b303b|->  notes.txt                             \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn choosing_a_file_from_the_picker_jumps_the_cursor_to_its_header() {
        // Stepping down to the second file and activating closes the picker and
        // lands the cursor on that file's header row.
        let mut app = App::new(document(), 8, &theme());
        app.update(Action::PickFile);
        app.picker_nav(Action::LineDown);
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.cursor(), 4);
    }

    #[test]
    fn cancelling_the_picker_leaves_the_cursor_where_it_was() {
        // Escaping the picker closes it without moving the cursor, even after
        // moving the highlight within it.
        let mut app = App::new(document(), 8, &theme());
        app.update(Action::PickFile);
        app.picker_nav(Action::LineDown);
        app.picker_cancel();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.cursor(), 0);
    }

    #[test]
    fn the_comment_picker_groups_comments_by_status_with_a_marker_location_and_author() {
        // Opening the picker over the commented review groups the comments by
        // status: the open one leads with a blank marker, then the resolved one
        // marked with a check, each showing its location and author before the
        // start of its body, then a spacer and the key hint.
        let mut app = App::reviewing(commented_review(), 8, &theme());
        app.update(Action::PickComment);
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump_picker(&mut app),
            "<#c0c5ce|#65737e|->> <#c0c5ce|#65737e|->  <#c0c5ce|#65737e|->#2  src/lib.rs:2  wez   why 2?<#c0c5ce|#65737e|->      \n",
            "<#c0c5ce|#2b303b|->  <#767b84|#2b303b|->✓ <#c0c5ce|#2b303b|->#1  src/lib.rs:1  opus  ok<#c0c5ce|#2b303b|->          \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn the_comment_picker_marks_each_status_and_strikes_through_a_withdrawn_comment() {
        // A review whose four comments span the statuses: an open one, a
        // resolved one, an open one whose anchor has drifted, and one withdrawn
        // as an uncommitted draft. The picker groups them draft first, then the
        // open pair in document order, then the resolved one, marking each with
        // its status glyph and striking through the withdrawn one.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Added, "one", 1),
                    (LineKind::Added, "two", 2),
                    (LineKind::Added, "three", 3),
                    (LineKind::Added, "four", 4),
                ],
            )],
        };
        let open = line_comment(
            1,
            ("wez", AuthorKind::Human),
            "src/lib.rs",
            1,
            "open one",
            false,
        );
        let resolved = line_comment(
            2,
            ("opus", AuthorKind::Agent),
            "src/lib.rs",
            2,
            "resolved one",
            true,
        );
        let mut shifted = line_comment(
            3,
            ("wez", AuthorKind::Human),
            "src/lib.rs",
            3,
            "shifted one",
            false,
        );
        shifted.confidence = Some(Confidence::Approximate);
        let withdrawn = line_comment(
            4,
            ("wez", AuthorKind::Human),
            "src/lib.rs",
            4,
            "withdrawn one",
            false,
        );
        let mut review = Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            vec![open, resolved, shifted, withdrawn],
            None,
        );
        review.toggle_deleted(Ulid(4));
        let mut app = App::reviewing(review, 8, &theme());
        app.update(Action::PickComment);
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump_picker(&mut app),
            "<#c0c5ce|#65737e|->> <#a3be8c|#65737e|->* <#767b84|#65737e|s>#4  src/lib.rs:4  wez   withdrawn one\n",
            "<#c0c5ce|#2b303b|->  <#c0c5ce|#2b303b|->  <#c0c5ce|#2b303b|->#1  src/lib.rs:1  wez   open one<#c0c5ce|#2b303b|->     \n",
            "<#c0c5ce|#2b303b|->  <#d08770|#2b303b|->! <#c0c5ce|#2b303b|->#3  src/lib.rs:3  wez   shifted one<#c0c5ce|#2b303b|->  \n",
            "<#c0c5ce|#2b303b|->  <#767b84|#2b303b|->✓ <#c0c5ce|#2b303b|->#2  src/lib.rs:2  opus  resolved one<#c0c5ce|#2b303b|-> \n",
            "<-|#2b303b|->                                         \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel \n",
        );
    }

    #[test]
    fn choosing_a_comment_from_the_picker_jumps_the_cursor_to_its_header() {
        // Stepping down past the leading open comment to the resolved one and
        // activating closes the picker and moves the cursor to that comment's
        // header row.
        let mut app = App::reviewing(commented_review(), 8, &theme());
        app.update(Action::PickComment);
        app.picker_nav(Action::LineDown);
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(
            app.kind_at(app.cursor()),
            Some(&RowKind::CommentHeader {
                id: BoxId::Comment(Ulid(1))
            })
        );
    }

    #[test]
    fn the_theme_picker_lists_every_theme_with_the_current_one_highlighted() {
        // Opening the picker over a review lists the bundled themes in name
        // order, opening on the one in effect rather than the first, then a
        // spacer and the key hint.
        let mut app = App::reviewing(commented_review(), 8, &theme());
        app.update(Action::PickTheme);
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_picker(&mut app),
            "<#c0c5ce|#2b303b|->  InspiredGitHub                        \n",
            "<#c0c5ce|#2b303b|->  Solarized (dark)                      \n",
            "<#c0c5ce|#2b303b|->  Solarized (light)                     \n",
            "<#c0c5ce|#2b303b|->  base16-eighties.dark                  \n",
            "<#c0c5ce|#2b303b|->  base16-mocha.dark                     \n",
            "<#c0c5ce|#65737e|->> base16-ocean.dark                     \n",
            "<#c0c5ce|#2b303b|->  base16-ocean.light                    \n",
            "<#c0c5ce|#2b303b|->  wez                                   \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn choosing_a_theme_from_the_picker_recolors_the_whole_view() {
        // Activating the light theme closes the picker and recolors the view:
        // the background and status bar take the light palette, and the diff
        // re-renders with it.
        let mut app = App::reviewing(commented_review(), 8, &theme());
        app.update(Action::PickTheme);
        app.picker_nav(Action::Top);
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        let light = Theme::light();
        wince::assert_eq!(app.background(), light.background);
    }

    /// A review over a one-line file whose latest captured version is `version`,
    /// for the version-comparison picker.
    fn versioned_review(version: u32) -> Review {
        Review::new(
            DiffView::new(theme()).unwrap(),
            Diff {
                files: vec![file(
                    "src/lib.rs",
                    FileStatus::Modified,
                    &[(LineKind::Added, "let y = 2;", 1)],
                )],
            },
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version,
            Vec::new(),
            None,
        )
    }

    #[test]
    fn the_compare_picker_lists_the_latest_diff_and_each_earlier_version() {
        // A review whose latest version is v2 offers the latest diff plus the
        // two earlier versions as reference points, newest first, opening on the
        // latest diff since no comparison is in effect.
        let mut app = App::reviewing(versioned_review(2), 8, &theme());
        app.update(Action::CompareVersions);
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_picker(&mut app),
            "<#c0c5ce|#65737e|->> the latest diff (v2) (showing now)    \n",
            "<#c0c5ce|#2b303b|->  changes since v1                      \n",
            "<#c0c5ce|#2b303b|->  changes since v0                      \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn the_compare_picker_marks_the_earlier_version_a_comparison_is_showing() {
        // With a comparison against v1 already in effect, the showing-now mark
        // follows it onto the earlier row rather than staying on the latest diff,
        // and the list opens on that row.
        let mut app = App::reviewing(versioned_review(2), 8, &theme());
        app.show_comparison(comparison_diff(), Some((1, from_v1_before_origin())));
        app.update(Action::CompareVersions);
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_picker(&mut app),
            "<#c0c5ce|#2b303b|->  the latest diff (v2)                  \n",
            "<#c0c5ce|#65737e|->> changes since v1 (showing now)        \n",
            "<#c0c5ce|#2b303b|->  changes since v0                      \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn choosing_an_earlier_version_records_the_comparison_request() {
        // Stepping down to the first earlier version and activating closes the
        // picker and records a request to compare against v1, which the host
        // reads back to reconstruct the diff.
        let mut app = App::reviewing(versioned_review(2), 8, &theme());
        app.update(Action::CompareVersions);
        app.picker_nav(Action::LineDown);
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.take_pending_compare(), Some(CompareRequest::Version(1)));
        wince::assert_eq!(app.take_pending_compare(), None);
    }

    #[test]
    fn the_compare_picker_reports_when_there_is_no_earlier_version() {
        // At v0 there is nothing earlier to compare against, so the picker does
        // not open and the status line says so.
        let mut app = App::reviewing(versioned_review(0), 8, &theme());
        app.set_width(TEST_WIDTH);
        app.update(Action::CompareVersions);
        wince::assert_eq!(app.picking(), false);
        wince::snapshot_display!(
            status_text(&app),
            "no earlier version to compare against   "
        );
    }

    #[test]
    fn the_refresh_prompt_offers_to_recapture_or_keep_the_current_diff() {
        // The launch prompt names the version the source has moved past and
        // offers the two choices, opening on the first.
        let mut app = App::reviewing(versioned_review(1), 8, &theme());
        app.offer_refresh();
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_picker(&mut app),
            "<#c0c5ce|#65737e|->> Refresh now                           \n",
            "<#c0c5ce|#2b303b|->  Keep the current diff                 \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn choosing_refresh_from_the_prompt_records_the_request() {
        // Activating the first choice closes the prompt and records the refresh
        // for the host to act on, taken exactly once.
        let mut app = App::reviewing(versioned_review(1), 8, &theme());
        app.offer_refresh();
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.take_pending_refresh(), true);
        wince::assert_eq!(app.take_pending_refresh(), false);
    }

    #[test]
    fn keeping_the_current_diff_from_the_prompt_records_no_refresh() {
        // Stepping to the second choice and activating closes the prompt without
        // asking the host to refresh.
        let mut app = App::reviewing(versioned_review(1), 8, &theme());
        app.offer_refresh();
        app.picker_nav(Action::LineDown);
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.take_pending_refresh(), false);
    }

    #[test]
    fn the_post_refresh_prompt_marks_the_latest_when_the_reviewer_was_on_the_latest() {
        // Refreshing while on the latest diff opens the list marking and opening
        // on the latest, where the reviewer was.
        let mut app = App::reviewing(versioned_review(1), 8, &theme());
        app.offer_compare_after_refresh(None, None);
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_picker(&mut app),
            "<#c0c5ce|#65737e|->> the latest diff (v1) (showing now)    \n",
            "<#c0c5ce|#2b303b|->  changes since v0                      \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn the_post_refresh_prompt_marks_and_opens_on_the_reviewers_prior_comparison() {
        // Refreshing while comparing against v1 opens the list marking and
        // opening on that row, so the reviewer keeps the perspective they had.
        let mut app = App::reviewing(versioned_review(2), 8, &theme());
        app.offer_compare_after_refresh(Some(1), None);
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_picker(&mut app),
            "<#c0c5ce|#2b303b|->  the latest diff (v2)                  \n",
            "<#c0c5ce|#65737e|->> changes since v1 (showing now)        \n",
            "<#c0c5ce|#2b303b|->  changes since v0                      \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn the_post_refresh_prompt_marks_where_the_reviewer_last_committed() {
        // Refreshing while on the latest with comments committed against v1 opens
        // on the latest, where the reviewer was, and marks v1 as where their
        // comments are so they can step to it.
        let mut app = App::reviewing(versioned_review(2), 8, &theme());
        app.offer_compare_after_refresh(None, Some(1));
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_picker(&mut app),
            "<#c0c5ce|#65737e|->> the latest diff (v2) (showing now)    \n",
            "<#c0c5ce|#2b303b|->  changes since v1 (your last comments) \n",
            "<#c0c5ce|#2b303b|->  changes since v0                      \n",
            "<-|#2b303b|->                                        \n",
            "<#767b84|#2b303b|->  up/down move  enter select  esc cancel\n",
        );
    }

    #[test]
    fn choosing_a_version_after_refresh_records_the_comparison() {
        // Stepping to the earlier version and activating closes the prompt and
        // records a request to compare against v0, taken exactly once.
        let mut app = App::reviewing(versioned_review(1), 8, &theme());
        app.offer_compare_after_refresh(None, None);
        app.picker_nav(Action::LineDown);
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.take_pending_compare(), Some(CompareRequest::Version(0)));
        wince::assert_eq!(app.take_pending_compare(), None);
    }

    #[test]
    fn keeping_the_latest_after_refresh_records_a_return_to_the_latest() {
        // Activating the default choice closes the prompt and records a return
        // to the latest diff the refresh already reloaded.
        let mut app = App::reviewing(versioned_review(1), 8, &theme());
        app.offer_compare_after_refresh(None, None);
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.take_pending_compare(), Some(CompareRequest::Latest));
        wince::assert_eq!(app.take_pending_compare(), None);
    }

    #[test]
    fn cancelling_the_post_refresh_prompt_keeps_the_prior_comparison() {
        // Escaping the post-refresh list keeps the reviewer comparing against the
        // version they were on before the refresh, against the fresh capture,
        // taken exactly once.
        let mut app = App::reviewing(versioned_review(2), 8, &theme());
        app.offer_compare_after_refresh(Some(1), None);
        app.picker_cancel();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.take_pending_compare(), Some(CompareRequest::Version(1)));
        wince::assert_eq!(app.take_pending_compare(), None);
    }

    #[test]
    fn cancelling_the_post_refresh_prompt_from_the_latest_returns_to_the_latest() {
        // Escaping when the reviewer was on the latest before the refresh keeps
        // them on the latest, taken exactly once.
        let mut app = App::reviewing(versioned_review(2), 8, &theme());
        app.offer_compare_after_refresh(None, None);
        app.picker_cancel();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.take_pending_compare(), Some(CompareRequest::Latest));
        wince::assert_eq!(app.take_pending_compare(), None);
    }

    #[test]
    fn cancelling_the_compare_hotkey_picker_records_no_comparison() {
        // The compare hotkey's list has no cancel comparison, so escaping it
        // leaves the reviewer where they were with nothing recorded.
        let mut app = App::reviewing(versioned_review(2), 8, &theme());
        app.update(Action::CompareVersions);
        app.picker_cancel();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.take_pending_compare(), None);
    }

    #[test]
    fn next_hunk_lands_on_the_hunk_header_and_bottom_jumps_to_the_end() {
        let (cursor, _, _) = after(10, &[Action::NextHunk]);
        wince::assert_eq!(cursor, 1);

        let (cursor, _, _) = after(10, &[Action::Bottom]);
        // Seven rows total (a header and hunk header plus content for each
        // file); the last is the added line of the second file.
        wince::assert_eq!(cursor, 6);
    }

    #[test]
    fn space_advances_a_whole_page_like_less() {
        // On a height-3 view, each page-down slides the viewport by a full screen
        // and carries the cursor along, rather than nudging it one row.
        let (cursor, top, _) = after(3, &[Action::PageDown]);
        wince::assert_eq!(cursor, 3);
        wince::assert_eq!(top, 3);

        let (cursor, top, _) = after(3, &[Action::PageDown, Action::PageDown]);
        wince::assert_eq!(cursor, 6);
        wince::assert_eq!(top, 4);

        let (cursor, top, _) = after(3, &[Action::PageDown, Action::PageDown, Action::PageUp]);
        wince::assert_eq!(cursor, 3);
        wince::assert_eq!(top, 1);
    }

    #[test]
    fn a_non_navigation_action_is_passed_back_to_the_host() {
        let mut app = App::new(document(), 10, &theme());
        wince::assert_eq!(app.update(Action::Refresh), Update::Passed(Action::Refresh));
        wince::assert_eq!(app.update(Action::LineDown), Update::Handled);
    }

    #[test]
    fn quitting_a_clean_viewport_resolves_by_the_configured_default() {
        // With no drafts to lose, a keep default leaves at once with no dialog
        // and settles on keeping the session.
        let mut app = App::new(document(), 10, &theme()).with_exit_default(ExitDefault::Keep);
        wince::assert_eq!(app.update(Action::Quit), Update::Handled);
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.pending_exit(), Some(Exit::Commit));
    }

    #[test]
    fn a_prompt_default_opens_the_picker_and_the_choice_settles_the_exit() {
        // A prompt default with nothing buffered asks keep-or-remove; moving to
        // the second choice and confirming removes the session.
        let mut app = App::new(document(), 10, &theme()).with_exit_default(ExitDefault::Prompt);
        wince::assert_eq!(app.update(Action::Quit), Update::Handled);
        wince::assert_eq!(app.picking(), true);
        wince::assert_eq!(app.pending_exit(), None);
        app.picker_nav(Action::LineDown);
        app.picker_activate();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.pending_exit(), Some(Exit::Remove));
    }

    #[test]
    fn cancelling_the_exit_picker_returns_to_the_review() {
        // Escape closes the picker without choosing, leaving the exit unresolved.
        let mut app = App::new(document(), 10, &theme()).with_exit_default(ExitDefault::Prompt);
        app.update(Action::Quit);
        app.picker_cancel();
        wince::assert_eq!(app.picking(), false);
        wince::assert_eq!(app.pending_exit(), None);
    }

    #[test]
    fn long_unchanged_runs_collapse_into_fold_markers() {
        // The whole collapsed view: the headers, a leading fold, the kept
        // context and the change, and a trailing fold.
        let (cursor, top, visible) = drive(folded_document(), 12, &[]);
        wince::assert_eq!(cursor, 0);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#f6f6f8|#65737e|b>modified  notes.txt<-|#65737e|->                     \n",
            "<#96b5b4|-|->@@ -1,17 +1,17 @@\n",
            "<#767b84|-|->          ▸ [5 unchanged lines]  ctx05\n",
            "<#7d828c|-|->   6    6   <#c0c5ce|-|->ctx06\n",
            "<#7d828c|-|->   7    7   <#c0c5ce|-|->ctx07\n",
            "<#7d828c|-|->   8    8   <#c0c5ce|-|->ctx08\n",
            "<#9ea1a9|#414a4a|->        9 + <#c0c5ce|#414a4a|->change!<-|#414a4a|->                     \n",
            "<#7d828c|-|->  10   10   <#c0c5ce|-|->ctx09\n",
            "<#7d828c|-|->  11   11   <#c0c5ce|-|->ctx10\n",
            "<#7d828c|-|->  12   12   <#c0c5ce|-|->ctx11\n",
            "<#767b84|-|->          ▸ [5 unchanged lines]  ctx16\n",
        );
    }

    #[test]
    fn expanding_a_fold_reveals_its_hidden_rows() {
        // The leading fold marker is the third view row; expand it there.
        let (cursor, top, visible) = drive(
            folded_document(),
            6,
            &[Action::LineDown, Action::LineDown, Action::ToggleFold],
        );
        wince::assert_eq!(cursor, 2);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#c0c5ce|-|b>modified  notes.txt\n",
            "<#96b5b4|-|->@@ -1,17 +1,17 @@\n",
            "<#d8dadd|#65737e|->   1    1 ▾ <#f6f6f8|#65737e|->ctx01<-|#65737e|->                       \n",
            "<#7d828c|-|->   2    2 │ <#c0c5ce|-|->ctx02\n",
            "<#7d828c|-|->   3    3 │ <#c0c5ce|-|->ctx03\n",
            "<#7d828c|-|->   4    4 │ <#c0c5ce|-|->ctx04\n",
        );
    }

    #[test]
    fn collapsing_an_expanded_fold_restores_its_marker() {
        // Expand the leading fold, then toggle it shut again from within it.
        let (cursor, top, visible) = drive(
            folded_document(),
            6,
            &[
                Action::LineDown,
                Action::LineDown,
                Action::ToggleFold,
                Action::ToggleFold,
            ],
        );
        wince::assert_eq!(cursor, 2);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#c0c5ce|-|b>modified  notes.txt\n",
            "<#96b5b4|-|->@@ -1,17 +1,17 @@\n",
            "<#cfd1d4|#65737e|->          ▸ [5 unchanged lines]  ctx05<-|#65737e|->  \n",
            "<#7d828c|-|->   6    6   <#c0c5ce|-|->ctx06\n",
            "<#7d828c|-|->   7    7   <#c0c5ce|-|->ctx07\n",
            "<#7d828c|-|->   8    8   <#c0c5ce|-|->ctx08\n",
        );
    }

    #[test]
    fn comments_open_with_the_resolved_one_collapsed_and_the_rest_expanded() {
        // The resolved comment on line 1 shows only its header; the unresolved
        // comment on line 2 shows its header and both body lines.
        let (cursor, top, visible) = drive(commented_document(), 12, &[]);
        wince::assert_eq!(cursor, 0);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#fcf7ee|#65737e|b>Review<#f7f7f8|#65737e|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#2 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->why 2? say more<-|-|->                       <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn next_and_prev_comment_jump_between_comment_headers() {
        // Two comment headers; next-comment lands on the first then the second,
        // and prev-comment walks back, staying put once past the first.
        let (cursor, _, _) = drive(commented_document(), 12, &[Action::NextComment]);
        wince::assert_eq!(cursor, 3);
        let (cursor, _, _) = drive(
            commented_document(),
            12,
            &[Action::NextComment, Action::NextComment],
        );
        wince::assert_eq!(cursor, 6);
        let (cursor, _, _) = drive(
            commented_document(),
            12,
            &[
                Action::NextComment,
                Action::NextComment,
                Action::PrevComment,
                Action::PrevComment,
            ],
        );
        wince::assert_eq!(cursor, 3);
    }

    #[test]
    fn toggling_a_comment_hides_and_restores_its_body() {
        // Land on the unresolved comment, collapse it so only its header shows,
        // then expand it again to reveal both body lines.
        let (cursor, top, visible) = drive(
            commented_document(),
            12,
            &[
                Action::NextComment,
                Action::NextComment,
                Action::ToggleComment,
            ],
        );
        wince::assert_eq!(cursor, 6);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#cfd1d4|#65737e|->┌ <#f9fafb|#65737e|->#2 wez (human)<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#cfd1d4|#65737e|-> <#cfd1d4|#65737e|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );

        let (cursor, _, visible) = drive(
            commented_document(),
            12,
            &[
                Action::NextComment,
                Action::NextComment,
                Action::ToggleComment,
                Action::ToggleComment,
            ],
        );
        wince::assert_eq!(cursor, 6);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#cfd1d4|#65737e|->┌ <#f9fafb|#65737e|->#2 wez (human)<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#cfd1d4|#65737e|-> <#cfd1d4|#65737e|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->why 2? say more<-|-|->                       <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn hiding_comments_drops_every_annotation_and_showing_restores_them() {
        // Move to the unresolved comment, then hide comments: every box drops
        // from the view and the cursor comes off the vanished comment onto the
        // code line it anchored, leaving just the diff.
        let (cursor, top, visible) = drive(
            commented_document(),
            12,
            &[
                Action::NextComment,
                Action::NextComment,
                Action::HideComments,
            ],
        );
        wince::assert_eq!(cursor, 4);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f5f6f6|#65737e|->        2 + <#faf7f9|#65737e|->let<#f6f6f8|#65737e|-> y <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->2<#f6f6f8|#65737e|->;<-|#65737e|->                  \n",
        );

        // Showing them again brings every box back; the cursor stays on the code.
        let (cursor, _, visible) = drive(
            commented_document(),
            12,
            &[
                Action::NextComment,
                Action::NextComment,
                Action::HideComments,
                Action::HideComments,
            ],
        );
        wince::assert_eq!(cursor, 9);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#2 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->why 2? say more<-|-|->                       <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#f5f6f6|#65737e|->        2 +<#fafafa|#65737e|->└<#f5f6f6|#65737e|-><#faf7f9|#65737e|->let<#f6f6f8|#65737e|-> y <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->2<#f6f6f8|#65737e|->;<-|#65737e|->                  \n",
        );
    }

    #[test]
    fn resolving_the_focused_comment_badges_it_as_a_resolved_draft() {
        // Land on the unresolved comment and resolve it: it gains a draft badge
        // ahead of the resolved one, and the cursor stays on its header.
        let (cursor, top, visible) = drive_review(
            commented_review(),
            12,
            &[
                Action::NextComment,
                Action::NextComment,
                Action::ResolveComment,
            ],
        );
        wince::assert_eq!(cursor, 6);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->#2 wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|-> [resolved by wez]<#cfd1d4|#65737e|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->why 2? say more<-|-|->                       <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn deleting_the_focused_comment_collapses_it_shown_as_a_deleted_draft() {
        // Land on the unresolved comment and delete it: it stays in the view as a
        // collapsed [draft] [deleted] header rather than vanishing, so the
        // deletion is visible and reversible.
        let (cursor, top, visible) = drive_review(
            commented_review(),
            12,
            &[
                Action::NextComment,
                Action::NextComment,
                Action::DeleteComment,
            ],
        );
        wince::assert_eq!(cursor, 6);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->#2 wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|-> [deleted by wez]<#cfd1d4|#65737e|->  press e to edit  x to resolve  d to undelete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn deleting_then_deleting_again_restores_the_focused_comment() {
        // A second delete on a deleted comment undoes it: the body returns and
        // no draft badge remains, since the comment is back to its committed
        // state.
        let (cursor, top, visible) = drive_review(
            commented_review(),
            12,
            &[
                Action::NextComment,
                Action::NextComment,
                Action::DeleteComment,
                Action::DeleteComment,
            ],
        );
        wince::assert_eq!(cursor, 6);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#cfd1d4|#65737e|->┌ <#f9fafb|#65737e|->#2 wez (human)<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#cfd1d4|#65737e|-> <#cfd1d4|#65737e|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->why 2? say more<-|-|->                       <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn an_editing_action_passes_through_when_no_review_is_attached() {
        // Without a review the app is a read-only viewport, so resolve and
        // delete are handed back to the host untouched.
        let mut app = App::new(commented_document(), 12, &theme());
        wince::assert_eq!(
            app.update(Action::ResolveComment),
            Update::Passed(Action::ResolveComment)
        );
        wince::assert_eq!(
            app.update(Action::DeleteComment),
            Update::Passed(Action::DeleteComment)
        );
        wince::assert_eq!(
            app.update(Action::AddComment),
            Update::Passed(Action::AddComment)
        );
        wince::assert_eq!(
            app.update(Action::EditComment),
            Update::Passed(Action::EditComment)
        );
    }

    #[test]
    fn adding_a_comment_on_a_line_renders_it_as_a_draft() {
        // Move onto the added line, author a comment there, and save it: it
        // appears as a pending draft in a block above the line it anchors.
        let mut app = App::reviewing(plain_review(), 12, &theme());
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        wince::assert_eq!(app.composing(), true);
        typed(&mut app, "why 2?");
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->why 2?<-|-|->                                <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn a_line_selection_washes_the_marked_span() {
        // Start a selection on the third line and extend it down two more: the
        // three marked lines wash in the cursor color as one block, while the
        // lines outside the selection keep their plain tint.
        let (cursor, top, visible) = drive_review(
            tall_review(6),
            16,
            &[
                Action::Top,
                Action::LineDown,
                Action::LineDown,
                Action::LineDown,
                Action::SelectLines,
                Action::LineDown,
                Action::LineDown,
            ],
        );
        wince::assert_eq!(cursor, 5);
        wince::assert_eq!(top, 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,6 +1,6 @@\n",
            "<#f5f6f6|#65737e|->        1 + <#faf7f9|#65737e|->let<#f6f6f8|#65737e|-> v1 <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->1<#f6f6f8|#65737e|->;<-|#65737e|->                 \n",
            "<#f5f6f6|#65737e|->        2 + <#faf7f9|#65737e|->let<#f6f6f8|#65737e|-> v2 <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->2<#f6f6f8|#65737e|->;<-|#65737e|->                 \n",
            "<#f5f6f6|#65737e|->        3 + <#faf7f9|#65737e|->let<#f6f6f8|#65737e|-> v3 <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->3<#f6f6f8|#65737e|->;<-|#65737e|->                 \n",
            "<#9ea1a9|#414a4a|->        4 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v4 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->4<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        5 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v5 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->5<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        6 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v6 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->6<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
        );
    }

    #[test]
    fn a_cross_file_jump_abandons_the_selection() {
        // A selection is held within one file. Starting one in the first file
        // and then jumping to the next file abandons it: the marked line no
        // longer washes, and the cursor wash sits only on the second file's
        // header, where the cursor now rests.
        let (cursor, top, visible) = drive(
            document(),
            6,
            &[
                Action::LineDown,
                Action::LineDown,
                Action::SelectLines,
                Action::NextFile,
            ],
        );
        wince::assert_eq!(cursor, 4);
        wince::assert_eq!(top, 1);
        #[rustfmt::skip]
        wince::snapshot_display!(
            visible,
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
            "<#f6f6f8|#65737e|b>added  notes.txt<-|#65737e|->                        \n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->hello<-|#414a4a|->                       \n",
        );
    }

    #[test]
    fn a_comment_over_a_selection_anchors_the_whole_range() {
        // Mark a three-line selection and author a comment: it anchors to the
        // whole span, its box tee joins the anchor rail, and the rail traces the
        // body glyph down the covered lines to the closing corner on the last.
        let mut app = App::reviewing(tall_review(6), 16, &theme());
        app.update(Action::Top);
        for _ in 0..3 {
            app.update(Action::LineDown);
        }
        app.update(Action::SelectLines);
        app.update(Action::LineDown);
        app.update(Action::LineDown);
        app.update(Action::AddComment);
        typed(&mut app, "extract a helper");
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,6 +1,6 @@\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->extract a helper<-|-|->                      <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        1 +<#a8c192|#414a4a|->│<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v1 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->1<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->│<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v2 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        3 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v3 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->3<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        4 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v4 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->4<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        5 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v5 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->5<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        6 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v6 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->6<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
        );
    }

    #[test]
    fn drafting_over_a_selection_previews_the_rail_before_saving() {
        // With the editor open over a three-line selection, the anchored lines
        // below it already trace the draft-colored rail down to the closing
        // corner, so the reviewer sees the covered span before saving.
        let mut app = App::reviewing(tall_review(6), 16, &theme());
        app.set_width(TEST_WIDTH);
        app.update(Action::Top);
        for _ in 0..3 {
            app.update(Action::LineDown);
        }
        app.update(Action::SelectLines);
        app.update(Action::LineDown);
        app.update(Action::LineDown);
        app.update(Action::AddComment);
        typed(&mut app, "extract a helper");
        wince::assert_eq!(app.composing(), true);
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_compose(&view),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,6 +1,6 @@\n",
            "--editor cursor 16,0--\n",
            "<#c0c5ce|-|->extract a helper\n",
            "--below--\n",
            "<#9ea1a9|#414a4a|->        1 +<#a8c192|#414a4a|->│<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v1 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->1<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->│<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v2 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        3 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v3 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->3<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        4 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v4 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->4<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        5 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v5 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->5<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
            "<#9ea1a9|#414a4a|->        6 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> v6 <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->6<#c0c5ce|#414a4a|->;<-|#414a4a|->                 \n",
        );
    }

    #[test]
    fn drafting_over_an_interleaved_removed_line_previews_an_unbroken_rail() {
        // The selected after-side span has a removed line woven through it. The
        // preview rail traces the body glyph through that removed row too, so
        // the covered span reads as one unbroken stroke before saving.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Added, "let a = 1;", 1),
                    (LineKind::Removed, "let gone = 0;", 2),
                    (LineKind::Added, "let b = 2;", 2),
                    (LineKind::Added, "let c = 3;", 3),
                ],
            )],
        };
        let review = Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            Vec::new(),
            None,
        );
        let mut app = App::reviewing(review, 16, &theme());
        app.set_width(TEST_WIDTH);
        app.update(Action::Top);
        for _ in 0..3 {
            app.update(Action::LineDown);
        }
        app.update(Action::SelectLines);
        for _ in 0..3 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "extract a helper");
        wince::assert_eq!(app.composing(), true);
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_compose(&view),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,4 +1,4 @@\n",
            "--editor cursor 16,0--\n",
            "<#c0c5ce|-|->extract a helper\n",
            "--below--\n",
            "<#9ea1a9|#414a4a|->        1 +<#a8c192|#414a4a|->│<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> a <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->1<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
            "<#91959d|#463943|->   2      -<#a3be8c|#463943|->│<#91959d|#463943|-><#bf9fb9|#463943|->let<#c0c5ce|#463943|-> <#c0c5ce|#66444e|->gone<#c0c5ce|#463943|-> <#c0c5ce|#463943|->=<#c0c5ce|#463943|-> <#e3b7a9|#66444e|->0<#c0c5ce|#66444e|->;<-|#463943|->               \n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->│<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> <#e3e5e9|#5b695b|->b<#c0c5ce|#414a4a|-> <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#f3e1db|#5b695b|->2<#e3e5e9|#5b695b|->;<-|#414a4a|->                  \n",
            "<#9ea1a9|#414a4a|->        3 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> c <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->3<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn the_editor_renders_inline_above_the_anchored_line() {
        // With the editor open on the added line, the split places the anchored
        // line just below the editor and the seeded body sits in the editor.
        let mut app = App::reviewing(plain_review(), 8, &theme());
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "why 2?");
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_compose(&view),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "--editor cursor 6,0--\n",
            "<#c0c5ce|-|->why 2?\n",
            "--below--\n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn a_long_editor_body_soft_wraps_to_the_box_interior() {
        // A body wider than the box interior wraps at a space onto a second
        // interior row rather than scrolling sideways, and the cursor rests at
        // the end of the last wrapped row.
        let mut app = App::reviewing(plain_review(), 14, &theme());
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "the quick brown fox jumps over the lazy dog");
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        // The cursor rests at the end of the last wrapped row (column 8 of
        // "lazy dog" on row 1).
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&view.editor_rows),
            "<#c0c5ce|-|->the quick brown fox jumps over the \n",
            "<#c0c5ce|-|->lazy dog\n",
        );
        wince::assert_eq!(view.editor_cursor, Some((8, 1)));
    }

    #[test]
    fn moving_up_in_the_editor_follows_the_wrapped_rows() {
        // From the end of a wrapped body, Up moves onto the first visual row at
        // the same column rather than leaving the editor, so the cursor sits on
        // row 0 rather than row 1.
        let mut app = App::reviewing(plain_review(), 14, &theme());
        app.set_width(TEST_WIDTH);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "the quick brown fox jumps over the lazy dog");
        app.compose_key(KeyPress::new(Key::Up));
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&view.editor_rows),
            "<#c0c5ce|-|->the quick brown fox jumps over the \n",
            "<#c0c5ce|-|->lazy dog\n",
        );
        wince::assert_eq!(view.editor_cursor, Some((8, 0)));
    }

    #[test]
    fn a_tall_editor_body_is_capped_and_scrolls() {
        // A body taller than the cap is clamped and scrolled to keep the cursor
        // (at the end) in view, reporting the scroll extent so a scrollbar can
        // be drawn. Height 20 caps the interior at 4 rows; the eight-line body
        // shows its last four.
        let mut app = App::reviewing(plain_review(), 20, &theme());
        app.set_width(TEST_WIDTH);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for i in 1..=8 {
            typed(&mut app, &format!("line {i}"));
            if i < 8 {
                app.compose_key(KeyPress::new(Key::Enter));
            }
        }
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&view.editor_rows),
            "<#c0c5ce|-|->line 5\n",
            "<#c0c5ce|-|->line 6\n",
            "<#c0c5ce|-|->line 7\n",
            "<#c0c5ce|-|->line 8\n",
        );
        wince::assert_eq!(view.editor_cursor, Some((6, 3)));
        wince::assert_eq!(
            view.editor_scroll.map(|s| (s.offset, s.total)),
            Some((4, 8))
        );
    }

    /// The detach chord, ctrl-o.
    fn detach() -> KeyPress {
        KeyPress::with_modifiers(Key::Char('o'), true, false, false)
    }

    /// Open the editor on the added line of a `height`-row review, seeded with
    /// `seed`, its width set for wrapping.
    fn composing_on_added_line(height: usize, seed: &str) -> App {
        let mut app = App::reviewing(plain_review(), height, &theme());
        app.set_width(TEST_WIDTH);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, seed);
        app
    }

    /// A review over a one-file diff of `lines` added lines, for tests that need
    /// a diff taller than the viewport so the floating editor scrolls to an edge.
    fn tall_review(lines: u32) -> Review {
        let owned: Vec<(LineKind, String, u32)> = (1..=lines)
            .map(|i| (LineKind::Added, format!("let v{i} = {i};"), i))
            .collect();
        let rows: Vec<(LineKind, &str, u32)> = owned
            .iter()
            .map(|(kind, text, n)| (*kind, text.as_str(), *n))
            .collect();
        let diff = Diff {
            files: vec![file("src/lib.rs", FileStatus::Modified, &rows)],
        };
        Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            Vec::new(),
            None,
        )
    }

    #[test]
    fn a_short_body_insets_the_box_but_a_wide_one_that_would_wrap_taller_does_not() {
        // A short body wraps to one row at either width, so the box insets its
        // border a couple of columns to give the roaming cursor's tint margin
        // cells to read against.
        let mut app = composing_on_added_line(20, "why 2?");
        app.compose_key(detach());
        wince::assert_eq!((float_inset(&app), float_top(&app)), (Some(2), Some(4)));
        // A body that fits the wide interior on one row but overflows the
        // narrower inset interior keeps the wide box, so the inset never grows
        // the box: TEST_WIDTH is 40, the wide interior 38 and the inset one 34,
        // and 36 characters wrap to one wide row but two inset rows.
        let mut app = composing_on_added_line(20, &"x".repeat(36));
        app.compose_key(detach());
        wince::assert_eq!((float_inset(&app), float_top(&app)), (Some(0), Some(4)));
    }

    #[test]
    fn the_detach_binding_floats_the_editor_over_its_anchor() {
        // ctrl-o detaches the editor: it stops rendering inline and floats over
        // the anchored line rather than snapping to a screen edge, with the
        // cursor leaving it to sit on that line.
        let mut app = composing_on_added_line(8, "why 2?");
        app.compose_key(detach());
        wince::assert_eq!(app.composing(), true);
        wince::assert_eq!(app.compose_view(TEST_WIDTH).is_none(), true);
        wince::assert_eq!(float_top(&app), Some(4));
        wince::assert_eq!(float_has_cursor(&app), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_detached(&app, TEST_WIDTH, 8),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "--float row 4 cursor off--\n",
            "<#c0c5ce|-|->why 2?\n",
            "--float end--\n",
            "\n",
        );
    }

    #[test]
    fn scrolling_the_anchor_off_the_top_rests_the_editor_at_the_top_edge() {
        // Author near the top of a tall diff, detach, then scroll the diff down
        // until the anchor leaves the top: the box tracks it up and rests at the
        // top edge, row 0.
        let mut app = App::reviewing(tall_review(40), 10, &theme());
        app.set_width(TEST_WIDTH);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "why?");
        app.compose_key(detach());
        // While the anchor is on screen the box floats over it, not at an edge.
        wince::assert_eq!(float_top(&app), Some(4));
        for _ in 0..20 {
            app.compose_key(ch('j'));
        }
        wince::assert_eq!(float_top(&app), Some(0));
        wince::assert_eq!(float_has_cursor(&app), false);
    }

    #[test]
    fn scrolling_the_anchor_off_the_bottom_rests_the_editor_at_the_bottom_edge() {
        // Author far down a tall diff, detach, then scroll the diff up until the
        // anchor leaves the bottom: the box tracks it down and rests at the
        // bottom edge, height minus the box height.
        let mut app = App::reviewing(tall_review(40), 10, &theme());
        app.set_width(TEST_WIDTH);
        for _ in 0..30 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "why?");
        app.compose_key(detach());
        for _ in 0..20 {
            app.compose_key(ch('k'));
        }
        wince::assert_eq!(float_top(&app), Some(7));
        wince::assert_eq!(float_has_cursor(&app), false);
    }

    #[test]
    fn the_roaming_cursor_reports_the_box_row_it_sits_behind() {
        // With a multi-line editor floating over the diff, stepping the cursor
        // down through the rows the box covers reports each row in turn, so the
        // chrome can show the cursor tint around the editor; once the cursor
        // steps clear below the box the report clears and the cursor shows its
        // own full-width tint again.
        let mut app = App::reviewing(tall_review(40), 20, &theme());
        app.set_width(TEST_WIDTH);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for line in ["a", "b", "c", "d"] {
            typed(&mut app, line);
            if line != "d" {
                app.compose_key(KeyPress::new(Key::Enter));
            }
        }
        app.compose_key(detach());
        // The cursor sits on the anchor, the box's top border row.
        wince::assert_eq!(float_cursor_offset(&app), Some(0));
        let offsets: Vec<Option<u16>> = (0..6)
            .map(|_| {
                app.compose_key(ch('j'));
                float_cursor_offset(&app)
            })
            .collect();
        wince::assert_eq!(
            offsets,
            vec![Some(1), Some(2), Some(3), Some(4), Some(5), None]
        );
    }

    #[test]
    fn an_arrow_past_the_top_nudges_the_editor_loose() {
        // With the cursor on the editor's first row, Up has nowhere to go inside
        // the editor, so it detaches, floats over the anchor, and steps the diff
        // up.
        let mut app = composing_on_added_line(8, "why 2?");
        let anchor = app.cursor();
        app.compose_key(KeyPress::new(Key::Up));
        wince::assert_eq!(float_top(&app), Some(4));
        wince::assert_eq!(float_has_cursor(&app), false);
        wince::assert_eq!(app.cursor(), anchor - 1);
    }

    #[test]
    fn an_arrow_past_the_bottom_nudges_the_editor_loose() {
        // With the cursor on the editor's last row, Down has nowhere to go
        // inside the editor, so it detaches, floats over the anchor, and places
        // the diff cursor on the line just below the box's bottom border,
        // mirroring the top edge. The box is three rows tall over the
        // single-line body, so the cursor rests three rows past the anchor.
        let mut app = App::reviewing(tall_review(20), 20, &theme());
        app.set_width(TEST_WIDTH);
        for _ in 0..5 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "why 2?");
        let anchor = app.cursor();
        app.compose_key(KeyPress::new(Key::Down));
        wince::assert_eq!(float_has_cursor(&app), false);
        wince::assert_eq!(app.cursor(), anchor + 3);
    }

    #[test]
    fn nudging_off_a_bottom_pinned_editor_does_nothing() {
        // Compose on the last line of a tall diff. The anchor sits against the
        // bottom of the viewport, so the box would float pinned to that edge
        // with no diff row below its bottom border on screen. Down on the
        // editor's last row therefore does nothing: the editor stays inline and
        // the diff does not scroll, rather than flinging the anchor up the page.
        let mut app = App::reviewing(tall_review(20), 12, &theme());
        app.set_width(TEST_WIDTH);
        for _ in 0..20 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "why 2?");
        let before = (app.top, app.cursor());
        app.compose_key(KeyPress::new(Key::Down));
        wince::assert_eq!((app.top, app.cursor()), before);
        wince::assert_eq!(app.compose_float(TEST_WIDTH).is_none(), true);
        wince::assert_eq!(app.compose_view(TEST_WIDTH).is_some(), true);
    }

    #[test]
    fn nudging_stays_in_the_editor_when_disabled() {
        // With nudging off, Up on the first row is an ordinary editor move: the
        // editor stays anchored inline and never floats.
        let mut app = App::reviewing(plain_review(), 8, &theme()).with_nudge_to_detach(false);
        app.set_width(TEST_WIDTH);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "why 2?");
        app.compose_key(KeyPress::new(Key::Up));
        wince::assert_eq!(app.compose_float(TEST_WIDTH).is_none(), true);
        wince::assert_eq!(app.compose_view(TEST_WIDTH).is_some(), true);
    }

    #[test]
    fn navigation_scrolls_the_diff_while_the_editor_floats() {
        // Detached and outside, j moves the diff cursor and the editor keeps
        // floating with the cursor out of it.
        let mut app = composing_on_added_line(8, "why 2?");
        app.compose_key(detach());
        let start = app.cursor();
        // The anchor is the last diff row, so move up into the diff, then back.
        app.compose_key(ch('k'));
        wince::assert_eq!(app.cursor(), start - 1);
        wince::assert_eq!(float_has_cursor(&app), false);
        app.compose_key(ch('j'));
        wince::assert_eq!(app.cursor(), start);
        wince::assert_eq!(float_has_cursor(&app), false);
    }

    #[test]
    fn the_edit_binding_returns_the_cursor_to_the_floating_editor() {
        // From outside, e returns the cursor to the editor without moving the
        // diff, so the reviewer resumes typing where they left off.
        let mut app = composing_on_added_line(8, "why 2?");
        app.compose_key(detach());
        app.compose_key(ch('j'));
        let looked_at = app.cursor();
        app.compose_key(ch('e'));
        wince::assert_eq!(float_has_cursor(&app), true);
        wince::assert_eq!(app.cursor(), looked_at);
        // The editor still floats; it does not snap back inline.
        wince::assert_eq!(app.compose_view(TEST_WIDTH).is_none(), true);
    }

    #[test]
    fn a_printable_key_returns_to_the_editor_and_inserts() {
        // From outside, an unbound printable snaps back into the editor and
        // types itself; submitting then shows the appended text in the draft.
        let mut app = composing_on_added_line(8, "why 2?");
        app.compose_key(detach());
        // '5' is unbound in the review keymap, so it re-enters and inserts.
        app.compose_key(ch('5'));
        wince::assert_eq!(float_has_cursor(&app), true);
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->why 2?5<-|-|->                               <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn submitting_from_a_floating_editor_saves_the_comment() {
        // Submit works while detached: the draft is authored and the editor
        // closes, clearing the floating state.
        let mut app = composing_on_added_line(8, "why 2?");
        app.compose_key(detach());
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), false);
        wince::assert_eq!(app.compose_float(TEST_WIDTH).is_none(), true);
    }

    #[test]
    fn cancelling_a_clean_editor_closes_it_at_once() {
        // Escape with nothing typed leaves editing immediately with no draft.
        let mut app = App::reviewing(plain_review(), 12, &theme());
        app.update(Action::AddComment);
        wince::assert_eq!(app.composing(), true);
        app.compose_key(KeyPress::new(Key::Escape));
        wince::assert_eq!(app.composing(), false);
        wince::assert_eq!(app.take_drafts(), Vec::new());
    }

    #[test]
    fn cancelling_a_changed_editor_asks_before_discarding() {
        // Escape after typing does not close; it asks. Declining resumes
        // editing; escaping again and confirming discards without a draft.
        let mut app = App::reviewing(plain_review(), 12, &theme());
        app.update(Action::AddComment);
        typed(&mut app, "hmm");
        app.compose_key(KeyPress::new(Key::Escape));
        wince::assert_eq!(app.composing(), true);
        app.compose_key(ch('n'));
        wince::assert_eq!(app.composing(), true);
        app.compose_key(KeyPress::new(Key::Escape));
        app.compose_key(ch('y'));
        wince::assert_eq!(app.composing(), false);
        wince::assert_eq!(app.take_drafts(), Vec::new());
    }

    #[test]
    fn an_empty_body_is_discarded_on_save() {
        // Saving an untouched editor authors nothing.
        let mut app = App::reviewing(plain_review(), 12, &theme());
        app.update(Action::AddComment);
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), false);
        wince::assert_eq!(app.take_drafts(), Vec::new());
    }

    #[test]
    fn the_editor_submit_key_follows_the_configured_keymap() {
        // Rebind the editor's submit key to ctrl-g. The default ctrl-d then
        // reaches the text buffer instead of confirming, and the border hint
        // names the configured key, so the editor's keys route through config
        // like every other action.
        let overrides: KeymapOverrides = [(
            Action::SubmitComment,
            vec!["ctrl-g".parse::<Chord>().unwrap()],
        )]
        .into_iter()
        .collect();
        let keymap = Keymap::resolve_config(&overrides, false).unwrap();
        let mut app = App::reviewing(plain_review(), 12, &theme()).with_keymap(keymap);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        wince::snapshot_display!(app.editor_hint(), "ctrl-g submit  esc cancel");
        typed(&mut app, "why 2?");
        // The former submit key is now ordinary input, so the editor stays open.
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), true);
        // The configured key confirms the comment as a draft above its line.
        app.compose_key(KeyPress::with_modifiers(Key::Char('g'), true, false, false));
        wince::assert_eq!(app.composing(), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->why 2?<-|-|->                                <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn the_floating_editor_hint_names_the_detach_key() {
        // Inline, the hint names only submit and cancel. Floating, it also names
        // the detach key before cancel, reading `edit` while the cursor roams
        // the diff (the key returns it to the editor) and `navigate` once it is
        // back in the editor (the key hands it to the diff).
        let mut app = composing_on_added_line(8, "why 2?");
        wince::snapshot_display!(app.editor_hint(), "ctrl-d submit  esc cancel");
        app.compose_key(detach());
        wince::snapshot_display!(
            app.editor_hint(),
            "ctrl-d submit  [ctrl-o edit]  esc cancel"
        );
        app.compose_key(ch('e'));
        wince::snapshot_display!(
            app.editor_hint(),
            "ctrl-d submit  [ctrl-o navigate]  esc cancel"
        );
    }

    #[test]
    fn editing_a_comment_seeds_the_editor_and_rewrites_the_body() {
        // Land on the unresolved comment and edit it: the editor opens with its
        // current body, and saving a new body rewrites it, badged as a draft.
        let mut app = App::reviewing(commented_review(), 14, &theme());
        app.update(Action::NextComment);
        app.update(Action::NextComment);
        app.update(Action::EditComment);
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        // The editor opens seeded with the current body and the cursor resting
        // past its end, on column 8 of the second row.
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&view.editor_rows),
            "<#c0c5ce|-|->why 2?\n",
            "<#c0c5ce|-|->say more\n",
        );
        wince::assert_eq!(view.editor_cursor, Some((8, 1)));
        drop(view);
        // Replace the body: clear the two seeded lines, then type a new one.
        for _ in 0..20 {
            app.compose_key(KeyPress::new(Key::Backspace));
        }
        typed(&mut app, "use a constant");
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->#2 wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->use a constant<-|-|->                        <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn replying_to_a_comment_threads_the_draft_beneath_it() {
        // Move to the unresolved comment and reply: the editor opens empty below
        // the comment, and saving a body threads it beneath as a draft while the
        // parent stays put above.
        let mut app = App::reviewing(commented_review(), 14, &theme());
        app.update(Action::NextComment);
        app.update(Action::NextComment);
        app.update(Action::ReplyComment);
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        // The editor opens empty, titled for a reply, with the cursor at the
        // start of its only row.
        wince::snapshot_display!(dump(&view.editor_rows), "\n");
        wince::assert_eq!(view.editor_cursor, Some((0, 0)));
        drop(view);
        typed(&mut app, "agreed");
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#2 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->why 2? say more<-|-|->                       <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->agreed<-|-|->                                <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────────────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn a_draft_reply_threads_below_an_existing_committed_reply() {
        // A root comment already has one committed reply. A freshly drafted reply
        // sorts to the bottom of the thread, beneath the committed one, where it
        // will stay once committed rather than jumping under the root.
        let (diff, _) = commented_diff();
        let comments = vec![
            line_comment(
                2,
                ("wez", AuthorKind::Human),
                "src/lib.rs",
                2,
                "why 2?",
                false,
            ),
            reply_comment(3, ("opus", AuthorKind::Agent), 2, "because", 5),
        ];
        let review = Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            comments,
            None,
        );
        let mut app = App::reviewing(review, 14, &theme());
        app.update(Action::NextComment);
        app.update(Action::ReplyComment);
        typed(&mut app, "there we go");
        app.compose_key(submit());
        wince::assert_eq!(app.composing(), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#2 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->why 2?<-|-|->                                <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#3 opus (agent)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->because<-|-|->                               <#767b84|-|->│\n",
            "<#767b84|-|->└──────────────────────────────────────┘\n",
            "<#f6f9f4|#65737e|->┌ <#f9fafb|#65737e|->wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->there we go<-|-|->                           <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────────────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn a_reply_is_refused_on_a_withdrawn_comment() {
        // Withdraw the unresolved comment, then attempt a reply: no editor opens
        // because a withdrawn comment cannot take a reply.
        let mut app = App::reviewing(commented_review(), 14, &theme());
        app.update(Action::NextComment);
        app.update(Action::NextComment);
        app.update(Action::DeleteComment);
        wince::assert_eq!(app.update(Action::ReplyComment), Update::Handled);
        wince::assert_eq!(app.composing(), false);
    }

    #[test]
    fn editing_a_comment_hides_its_rendered_box_behind_the_editor() {
        // Editing the unresolved comment drops its rendered box from the split:
        // the editor stands where the box was, the resolved comment above it
        // stays, and the anchored code sits just below the editor.
        let mut app = App::reviewing(commented_review(), 14, &theme());
        app.update(Action::NextComment);
        app.update(Action::NextComment);
        app.update(Action::EditComment);
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_compose(&view),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "--editor cursor 8,1--\n",
            "<#c0c5ce|-|->why 2?\n",
            "<#c0c5ce|-|->say more\n",
            "--below--\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn a_new_review_comment_opens_below_the_existing_review_comments() {
        // With a review comment already placed under the summary, authoring
        // another opens the editor below it, where its box will render, rather
        // than wedged between the summary and the existing comment.
        let mut app = App::reviewing(commented_review(), 12, &theme());
        app.update(Action::AddComment);
        typed(&mut app, "first");
        app.compose_key(submit());
        app.update(Action::Top);
        app.update(Action::AddComment);
        typed(&mut app, "second");
        let view = app.compose_view(TEST_WIDTH).expect("composing");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump_compose(&view),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#a3be8c|-|->┌ <#8fa1b3|-|->wez (human)<#a3be8c|-|-> [draft]<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#a3be8c|-|-> <#a3be8c|-|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->first<-|-|->                                 <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────────────────────────────────┘\n",
            "--editor cursor 6,0--\n",
            "<#c0c5ce|-|->second\n",
            "--below--\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
        );
    }

    /// A review whose description is already committed, for exercising the box
    /// an existing description renders as.
    fn described_review() -> Review {
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
        Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            Vec::new(),
            Some(DescriptionState {
                content: Description {
                    title: "Tidy the parser".to_string(),
                    body: "Split the lexer out.".to_string(),
                },
                author: Author {
                    name: "opus".to_string(),
                    kind: AuthorKind::Agent,
                },
                updated_at: OffsetDateTime::UNIX_EPOCH,
                origin: None,
                synced_marker: None,
            }),
        )
    }

    #[test]
    fn writing_the_description_from_the_summary_row_shows_it_as_a_box() {
        // A review opens with no description, so the summary row offers the write
        // hint and edit on that row opens the description editor. Submitting a
        // commit-message-shaped body renders it as the Description box leading
        // the review and buffers a single description record to commit.
        let mut app = App::reviewing(plain_review(), 12, &theme());
        app.set_width(TEST_WIDTH);
        app.update(Action::Top);
        app.update(Action::EditComment);
        typed(&mut app, "Tidy the parser");
        app.compose_key(KeyPress::new(Key::Enter));
        app.compose_key(KeyPress::new(Key::Enter));
        typed(&mut app, "Split the lexer out.");
        app.compose_key(submit());
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]\n",
            "<#f6f9f4|#65737e|->┌ <#fcf7ee|#65737e|b>Description<#f9fafb|#65737e|->  wez (human)<#f6f9f4|#65737e|-> [draft]<#cfd1d4|#65737e|->  press e to edit  tab to expand/collapse<#f6f9f4|#65737e|-> <#f6f9f4|#65737e|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->Tidy the parser<-|-|->                       <#a3be8c|-|->│\n",
            "<#a3be8c|-|->│<-|-|->                                      <#a3be8c|-|->│\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->Split the lexer out.<-|-|->                  <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────────────────────────────────┘\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
        wince::assert_eq!(
            app.take_drafts(),
            vec![RecordBody::Description(DescriptionRecord {
                author: Author {
                    name: "wez".to_string(),
                    kind: AuthorKind::Human,
                },
                authored_at: None,
                origin: None,
                synced_marker: None,
                description: Description {
                    title: "Tidy the parser".to_string(),
                    body: "Split the lexer out.".to_string(),
                },
            })]
        );
    }

    #[test]
    fn a_committed_description_renders_as_a_box_and_edit_reseeds_it() {
        // A review that already has a description leads with the Description box
        // rather than the summary write hint, and editing it seeds the editor
        // with the committed title and body in commit-message form.
        let mut app = App::reviewing(described_review(), 12, &theme());
        app.set_width(TEST_WIDTH);
        // The box leads the review, titled Description and offering only the
        // edit and collapse keys since a description cannot be resolved or
        // deleted; the summary keeps its review-comment hint but drops the write
        // hint now that a description exists.
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#fcf7ee|#65737e|b>Review<#f7f7f8|#65737e|-> [press c here to draft the review comment]\n",
            "<#767b84|-|->┌ <#ebcb8b|-|b>Description<#8fa1b3|-|->  opus (agent)<#767b84|-|->  press e to edit  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->Tidy the parser<-|-|->                       <#767b84|-|->│\n",
            "<#767b84|-|->│<-|-|->                                      <#767b84|-|->│\n",
            "<#767b84|-|->│<#c0c5ce|-|->Split the lexer out.<-|-|->                  <#767b84|-|->│\n",
            "<#767b84|-|->└──────────────────────────────────────┘\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
        // Editing the description seeds the editor with the committed title and
        // body rejoined into their commit-message form.
        app.update(Action::Top);
        app.update(Action::LineDown);
        app.update(Action::EditComment);
        wince::assert_eq!(
            app.compose
                .as_ref()
                .map(|compose| compose.body())
                .unwrap_or_default(),
            "Tidy the parser\n\nSplit the lexer out.".to_string()
        );
    }

    /// The status-line text an app currently shows at `width`.
    fn status_text_at(app: &App, width: usize) -> String {
        app.status(width)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    /// The status-line text an app currently shows at the test width.
    fn status_text(app: &App) -> String {
        status_text_at(app, TEST_WIDTH)
    }

    #[test]
    fn refreshing_keeps_the_cursor_on_the_same_line_and_reports_the_tally() {
        // The reviewer sits on the added line, then a refresh appends a third
        // line below it. The same numbered line survives, so the cursor stays on
        // it, and the status line reports the captured version and comment tally.
        let mut app = App::reviewing(plain_review(), 12, &theme());
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        let new = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Context, "let x = 1;", 1),
                    (LineKind::Added, "let y = 2;", 2),
                    (LineKind::Added, "let z = 3;", 3),
                ],
            )],
        };
        app.refresh(new, Vec::new(), 1, |_| unreachable!("no drafts to rebase"))
            .unwrap();
        app.set_message(
            "captured v1; rebased 0 comments: 0 exact, 0 shifted, 0 outdated".to_string(),
        );

        wince::assert_eq!(app.cursor(), 4);
        wince::assert_eq!(app.top(), 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,3 +1,3 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f5f6f6|#65737e|->        2 + <#faf7f9|#65737e|->let<#f6f6f8|#65737e|-> y <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->2<#f6f6f8|#65737e|->;<-|#65737e|->                  \n",
            "<#9ea1a9|#414a4a|->        3 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> z <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->3<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
        // The status line truncates the note to the screen width.
        wince::snapshot_display!(
            status_text(&app),
            "captured v1; rebased 0 comments: 0 exact"
        );
    }

    #[test]
    fn refreshing_rebases_a_pending_draft_onto_the_new_diff() {
        // A draft is authored on the added line, then a refresh inserts a line
        // above it. The drafted comment moves forward with its line, so it
        // renders above the same code, now one line lower.
        let mut app = App::reviewing(plain_review(), 14, &theme());
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for c in "why?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        app.compose_key(submit());

        let old = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Context, "let x = 1;", 1),
                    (LineKind::Added, "let y = 2;", 2),
                ],
            )],
        };
        let new = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Added, "let a = 0;", 1),
                    (LineKind::Context, "let x = 1;", 2),
                    (LineKind::Added, "let y = 2;", 3),
                ],
            )],
        };
        app.refresh(new, Vec::new(), 1, |version| {
            wince::assert_eq!(version, 0);
            Ok(old.clone())
        })
        .unwrap();
        app.update(Action::Top);

        // The drafted comment sits above the added `let y = 2;`, which has moved
        // to the third line, and still wears its uncommitted draft badge.
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#fcf7ee|#65737e|b>Review<#f7f7f8|#65737e|-> [press c here to draft the review comment]<#f7f7f8|#65737e|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,3 +1,3 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> a <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->0<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
            "<#7d828c|-|->   2    2   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#a3be8c|-|->┌ <#8fa1b3|-|->wez (human)<#a3be8c|-|-> [draft]<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#a3be8c|-|-> <#a3be8c|-|->┐\n",
            "<#a3be8c|-|->│<#c0c5ce|-|->why?<-|-|->                                  <#a3be8c|-|->│\n",
            "<#a3be8c|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        3 +<#a8c192|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn refreshing_onto_a_missing_line_lands_on_the_nearest_survivor() {
        // The reviewer sits on the added second line, then a refresh drops it,
        // leaving only the first line. With the exact line gone, the cursor
        // falls back to the nearest surviving line in the same file.
        let mut app = App::reviewing(plain_review(), 12, &theme());
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        let new = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Context, "let x = 1;", 1)],
            )],
        };
        app.refresh(new, Vec::new(), 1, |_| unreachable!("no drafts to rebase"))
            .unwrap();

        wince::assert_eq!(app.cursor(), 3);
        wince::assert_eq!(app.top(), 0);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#d8dadd|#65737e|->   1    1   <#fbf9fb|#65737e|->let<#f6f6f8|#65737e|-> x <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fdf9f8|#65737e|->1<#f6f6f8|#65737e|->;<-|#65737e|->                  \n",
        );
    }

    #[test]
    fn a_side_by_side_after_comment_opens_a_box_scoped_to_the_right_column() {
        // Both comments anchor to after-side lines, so each box sits in the right
        // column with the left column blank and the divider running unbroken
        // between them. A narrow column clips the header title so the box keeps
        // its shape. The box's bottom-edge tee and the anchor rail in the content
        // row below sit in the same right-gutter column.
        let mut app = App::reviewing(commented_review(), 10, &theme())
            .with_diff_mode(DiffMode::SideBySide, 0);
        app.set_width(52);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(52)),
            "<#fcf7ee|#65737e|b>Review<#f7f7f8|#65737e|-> [press c here to draft the review comment]<#f7f7f8|#65737e|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<-|-|->                         <#7d828c|-|->│<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resol<#767b84|-|-> <#767b84|-|->┐\n",
            "<-|-|->                         <#7d828c|-|->│<#767b84|-|->└─────┬──────────────────┘\n",
            "<#7d828c|-|->   1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;<-|-|->        <#7d828c|-|->│<#7d828c|-|->   1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;<-|-|->         \n",
            "<-|-|->                         <#7d828c|-|->│<#767b84|-|->┌ <#8fa1b3|-|->#2 wez (human)<#767b84|-|->  press <#767b84|-|-> <#767b84|-|->┐\n",
            "<-|-|->                         <#7d828c|-|->│<#767b84|-|->│<#c0c5ce|-|->why 2? say more<-|-|->         <#767b84|-|->│\n",
            "<-|-|->                         <#7d828c|-|->│<#767b84|-|->└─────┬──────────────────┘\n",
            "<-|-|->                         <#7d828c|-|->│<#9ea1a9|#414a4a|->   2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->         \n",
        );
    }

    #[test]
    fn the_diff_mode_actions_switch_the_layout_and_keep_the_cursor() {
        // Reviewing unified with the cursor on the added line, the side-by-side
        // action splits the diff into two columns without moving the cursor: the
        // added line's selection wash follows it into the right column. The
        // unified action then folds it back to one column.
        let mut app = App::reviewing(plain_review(), 6, &theme());
        app.set_width(44);
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        let seat = app.cursor();

        app.update(Action::DiffModeSideBySide);
        wince::assert_eq!(app.cursor(), seat);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(44)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;<-|-|->    <#7d828c|-|->│<#7d828c|-|->   1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;<-|-|->     \n",
            "<-|#65737e|->                     <#d8dadd|#65737e|->│<#f5f6f6|#65737e|->   2 + <#faf7f9|#65737e|->let<#f6f6f8|#65737e|-> y <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->2<#f6f6f8|#65737e|->;<-|#65737e|->     \n",
        );

        app.update(Action::DiffModeUnified);
        wince::assert_eq!(app.cursor(), seat);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(44)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f5f6f6|#65737e|->        2 + <#faf7f9|#65737e|->let<#f6f6f8|#65737e|-> y <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->2<#f6f6f8|#65737e|->;<-|#65737e|->                      \n",
        );
    }

    #[test]
    fn a_side_by_side_search_washes_the_match_within_each_column() {
        // At a width that resolves to two columns, the context line shows on both
        // sides and the added line only on the right. Searching "let" washes the
        // match in whichever column holds it: both columns of the context row,
        // and the right column of the added row, past the divider rule that
        // separates the two content runs.
        let mut app =
            App::reviewing(plain_review(), 6, &theme()).with_diff_mode(DiffMode::SideBySide, 0);
        app.set_width(44);
        search_for(&mut app, Action::SearchForward, "let");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(44)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#d8dadd|#65737e|->   1   <#fbf9fb|#686255|->let<#f6f6f8|#65737e|-> x <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fdf9f8|#65737e|->1<#f6f6f8|#65737e|->;<-|#65737e|->    <#d8dadd|#65737e|->│<#d8dadd|#65737e|->   1   <#fbf9fb|#686255|->let<#f6f6f8|#65737e|-> x <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fdf9f8|#65737e|->1<#f6f6f8|#65737e|->;<-|#65737e|->     \n",
            "<-|-|->                     <#7d828c|-|->│<#9ea1a9|#414a4a|->   2 + <#e8dbe5|#686255|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->     \n",
        );
    }

    /// Open a search in `direction` and type `pattern` into the prompt.
    fn search_for(app: &mut App, direction: Action, pattern: &str) {
        app.update(direction);
        for c in pattern.chars() {
            app.search_key(ch(c));
        }
    }

    #[test]
    fn an_incremental_search_jumps_the_cursor_to_the_first_match() {
        // Typing a pattern moves the cursor onto the first matching line across
        // files while the prompt shows the term, the live match tally, and the
        // progress percent; accepting it keeps the cursor and adds the repeat
        // keys.
        let mut app = App::new(document(), 9, &theme());
        search_for(&mut app, Action::SearchForward, "hello");
        wince::snapshot_display!(
            status_text(&app),
            "/hello  1/1 matches                 100%"
        );
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
            "<#c0c5ce|-|b>added  notes.txt\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#f5f6f6|#65737e|->        1 + <#f6f6f8|#686255|->hello<-|#65737e|->                       \n",
        );
        app.search_key(KeyPress::new(Key::Enter));
        wince::assert_eq!(app.searching(), false);
        wince::snapshot_display!(
            status_text(&app),
            "/hello  n next  N prev  1/1 matches 100%"
        );
    }

    #[test]
    fn a_search_highlights_every_visible_occurrence_of_the_term() {
        // Typing a term that appears on two lines highlights both, the one the
        // cursor jumps to and the other still in view, each occurrence washed in
        // the match color while the rest of its row keeps its own tint.
        let mut app = App::new(document(), 9, &theme());
        search_for(&mut app, Action::SearchForward, "let");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#d8dadd|#65737e|->   1    1   <#fbf9fb|#686255|->let<#f6f6f8|#65737e|-> x <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fdf9f8|#65737e|->1<#f6f6f8|#65737e|->;<-|#65737e|->                  \n",
            "<#9ea1a9|#414a4a|->        2 + <#e8dbe5|#686255|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
            "<#c0c5ce|-|b>added  notes.txt\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->hello<-|#414a4a|->                       \n",
        );
    }

    #[test]
    fn a_search_with_no_match_shows_a_no_matches_tally() {
        // A term absent from the document reports no matches in the prompt while
        // the cursor stays where the search opened.
        let mut app = App::new(document(), 9, &theme());
        search_for(&mut app, Action::SearchForward, "absent");
        wince::snapshot_display!(
            status_text(&app),
            "/absent  no matches                   0%"
        );
    }

    #[test]
    fn a_regex_search_treats_the_pattern_as_an_expression() {
        // The pattern is a regular expression: the dot matches any character, so
        // `l.t` finds `let` on both changed lines, jumps the cursor to the first,
        // and highlights every occurrence in view while the tally counts them.
        let mut app = App::new(document(), 9, &theme());
        search_for(&mut app, Action::SearchForward, "l.t");
        wince::snapshot_display!(
            status_text(&app),
            "/l.t  1/2 matches                    33%"
        );
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#d8dadd|#65737e|->   1    1   <#fbf9fb|#686255|->let<#f6f6f8|#65737e|-> x <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fdf9f8|#65737e|->1<#f6f6f8|#65737e|->;<-|#65737e|->                  \n",
            "<#9ea1a9|#414a4a|->        2 + <#e8dbe5|#686255|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
            "<#c0c5ce|-|b>added  notes.txt\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->hello<-|#414a4a|->                       \n",
        );
    }

    #[test]
    fn a_half_typed_regex_reports_a_bad_pattern_rather_than_matching() {
        // An unbalanced group is not yet a valid expression, so the status says
        // so and the cursor stays where the search opened rather than jumping.
        let mut app = App::new(document(), 9, &theme());
        search_for(&mut app, Action::SearchForward, "(let");
        wince::snapshot_display!(
            status_text(&app),
            "/(let  bad pattern                    0%"
        );
    }

    #[test]
    fn a_search_passes_over_content_hidden_in_a_collapsed_fold() {
        // A term that lives only inside a collapsed fold finds nothing, so the
        // cursor stays where the search opened.
        let mut app = App::new(folded_document(), 20, &theme());
        search_for(&mut app, Action::SearchForward, "ctx03");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#f6f6f8|#65737e|b>modified  notes.txt<-|#65737e|->                     \n",
            "<#96b5b4|-|->@@ -1,17 +1,17 @@\n",
            "<#767b84|-|->          ▸ [5 unchanged lines]  ctx05\n",
            "<#7d828c|-|->   6    6   <#c0c5ce|-|->ctx06\n",
            "<#7d828c|-|->   7    7   <#c0c5ce|-|->ctx07\n",
            "<#7d828c|-|->   8    8   <#c0c5ce|-|->ctx08\n",
            "<#9ea1a9|#414a4a|->        9 + <#c0c5ce|#414a4a|->change!<-|#414a4a|->                     \n",
            "<#7d828c|-|->  10   10   <#c0c5ce|-|->ctx09\n",
            "<#7d828c|-|->  11   11   <#c0c5ce|-|->ctx10\n",
            "<#7d828c|-|->  12   12   <#c0c5ce|-|->ctx11\n",
            "<#767b84|-|->          ▸ [5 unchanged lines]  ctx16\n",
        );
    }

    #[test]
    fn a_search_matches_visible_context_around_a_change() {
        // A context line kept visible beside the change is matched and focused.
        let mut app = App::new(folded_document(), 20, &theme());
        search_for(&mut app, Action::SearchForward, "ctx07");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#c0c5ce|-|b>modified  notes.txt\n",
            "<#96b5b4|-|->@@ -1,17 +1,17 @@\n",
            "<#767b84|-|->          ▸ [5 unchanged lines]  ctx05\n",
            "<#7d828c|-|->   6    6   <#c0c5ce|-|->ctx06\n",
            "<#d8dadd|#65737e|->   7    7   <#f6f6f8|#686255|->ctx07<-|#65737e|->                       \n",
            "<#7d828c|-|->   8    8   <#c0c5ce|-|->ctx08\n",
            "<#9ea1a9|#414a4a|->        9 + <#c0c5ce|#414a4a|->change!<-|#414a4a|->                     \n",
            "<#7d828c|-|->  10   10   <#c0c5ce|-|->ctx09\n",
            "<#7d828c|-|->  11   11   <#c0c5ce|-|->ctx10\n",
            "<#7d828c|-|->  12   12   <#c0c5ce|-|->ctx11\n",
            "<#767b84|-|->          ▸ [5 unchanged lines]  ctx16\n",
        );
    }

    #[test]
    fn a_search_expands_a_collapsed_comment_to_reveal_a_match() {
        // The resolved comment opens collapsed, hiding its body. Searching a
        // word from that body expands the comment and lands the cursor on the
        // matched line.
        let mut app = App::reviewing(commented_review(), 14, &theme());
        search_for(&mut app, Action::SearchForward, "ok");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#cfd1d4|#65737e|->│<#f6f6f8|#686255|->ok<-|#65737e|->                                    <#cfd1d4|#65737e|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#2 wez (human)<#767b84|-|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->why 2? say more<-|-|->                       <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn a_search_jumps_to_a_comment_by_its_review_scoped_number() {
        // The header row is searchable by the number the title leads with, so
        // `#2 ` jumps the cursor onto that comment and washes the visible label.
        let mut app = App::reviewing(commented_review(), 14, &theme());
        search_for(&mut app, Action::SearchForward, "#2 ");
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#ebcb8b|#4f5b66|b>Review<#adb0b5|#4f5b66|-> [press c here to draft the review comment]<#adb0b5|#4f5b66|-> [press e to write the description]\n",
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#767b84|-|->┌ <#8fa1b3|-|->#1 opus (agent)<#767b84|-|-> [resolved]<#767b84|-|->  press e to edit  r to reply  x to unresolve  d to delete  tab to expand/collapse<#767b84|-|-> <#767b84|-|->┐\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#7d828c|-|->   1    1  <#767b84|-|->└<#7d828c|-|-><#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#cfd1d4|#65737e|->┌ <#f9fafb|#686255|->#2 <#f9fafb|#65737e|->wez (human)<#cfd1d4|#65737e|->  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse<#cfd1d4|#65737e|-> <#cfd1d4|#65737e|->┐\n",
            "<#767b84|-|->│<#c0c5ce|-|->why 2? say more<-|-|->                       <#767b84|-|->│\n",
            "<#767b84|-|->└──────────┬───────────────────────────┘\n",
            "<#9ea1a9|#414a4a|->        2 +<#989ca3|#414a4a|->└<#9ea1a9|#414a4a|-><#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
        );
    }

    #[test]
    fn cancelling_a_search_returns_the_cursor_to_where_it_opened() {
        // Escaping the prompt returns to the origin line the search opened on.
        let mut app = App::new(document(), 9, &theme());
        for _ in 0..2 {
            app.update(Action::LineDown);
        }
        search_for(&mut app, Action::SearchForward, "hello");
        app.search_key(KeyPress::new(Key::Escape));
        wince::assert_eq!(app.searching(), false);
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#d8dadd|#65737e|->   1    1   <#fbf9fb|#65737e|->let<#f6f6f8|#65737e|-> x <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fdf9f8|#65737e|->1<#f6f6f8|#65737e|->;<-|#65737e|->                  \n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
            "<#c0c5ce|-|b>added  notes.txt\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->hello<-|#414a4a|->                       \n",
        );
    }

    #[test]
    fn repeating_a_search_wraps_and_reports_hitting_the_end() {
        // With a single match, accepting the search and repeating it forward
        // wraps back to the same line. The status keeps the term, tally, and
        // percent and adds a wrap note alongside them, rather than replacing the
        // position with a full-width message. (Shown here at a wider width so the
        // note is not truncated away.)
        let mut app = App::new(document(), 9, &theme());
        search_for(&mut app, Action::SearchForward, "hello");
        app.search_key(KeyPress::new(Key::Enter));
        app.update(Action::SearchNext);
        wince::snapshot_display!(
            status_text_at(&app, 60),
            "/hello  n next  N prev  1/1 matches  wrapped to top     100%"
        );
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#9ea1a9|#414a4a|->        2 + <#cbb0c6|#414a4a|->let<#c0c5ce|#414a4a|-> y <#c0c5ce|#414a4a|->=<#c0c5ce|#414a4a|-> <#deab9b|#414a4a|->2<#c0c5ce|#414a4a|->;<-|#414a4a|->                  \n",
            "<#c0c5ce|-|b>added  notes.txt\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#f5f6f6|#65737e|->        1 + <#f6f6f8|#686255|->hello<-|#65737e|->                       \n",
        );
    }

    #[test]
    fn repeating_a_search_to_a_further_match_keeps_the_term_and_repeat_keys() {
        // With the term on two lines, accepting on the first and repeating
        // forward lands on the second without a wrap, so the status keeps
        // showing the accepted term and the repeat keys rather than a wrap note.
        let mut app = App::new(document(), 9, &theme());
        search_for(&mut app, Action::SearchForward, "let");
        app.search_key(KeyPress::new(Key::Enter));
        app.update(Action::SearchNext);
        wince::snapshot_display!(
            status_text(&app),
            "/let  n next  N prev  2/2 matches    50%"
        );
        #[rustfmt::skip]
        wince::snapshot_display!(
            dump(&app.visible(TEST_WIDTH)),
            "<#c0c5ce|-|b>modified  src/lib.rs\n",
            "<#96b5b4|-|->@@ -1,2 +1,2 @@\n",
            "<#7d828c|-|->   1    1   <#e9dde6|#686255|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;\n",
            "<#f5f6f6|#65737e|->        2 + <#faf7f9|#686255|->let<#f6f6f8|#65737e|-> y <#f6f6f8|#65737e|->=<#f6f6f8|#65737e|-> <#fcf7f5|#65737e|->2<#f6f6f8|#65737e|->;<-|#65737e|->                  \n",
            "<#c0c5ce|-|b>added  notes.txt\n",
            "<#96b5b4|-|->@@ -1,1 +1,1 @@\n",
            "<#9ea1a9|#414a4a|->        1 + <#c0c5ce|#414a4a|->hello<-|#414a4a|->                       \n",
        );
    }

    /// The plain text a reviewer sees on `app`'s screen at `width`: each visible
    /// row's span contents joined with trailing padding trimmed.
    fn plain(app: &App, width: usize) -> String {
        let mut out = String::new();
        for line in app.visible(width) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            out.push_str(text.trim_end());
            out.push('\n');
        }
        out
    }

    /// A comparison diff of one file: v1's after side (alpha/beta/gamma) on the
    /// left, v2's after side (alpha/BETA/gamma/delta) on the right.
    fn comparison_diff() -> Diff {
        use wiff_diff::{DiffLine, FileDiff, Hunk};
        Diff {
            files: vec![FileDiff {
                old_path: "f.txt".to_string(),
                new_path: "f.txt".to_string(),
                status: FileStatus::Modified,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_len: 3,
                    new_start: 1,
                    new_len: 4,
                    section: None,
                    lines: vec![
                        DiffLine {
                            kind: LineKind::Context,
                            text: "alpha".to_string(),
                            old_lineno: Some(ln(1)),
                            new_lineno: Some(ln(1)),
                        },
                        DiffLine {
                            kind: LineKind::Removed,
                            text: "beta".to_string(),
                            old_lineno: Some(ln(2)),
                            new_lineno: None,
                        },
                        DiffLine {
                            kind: LineKind::Added,
                            text: "BETA".to_string(),
                            old_lineno: None,
                            new_lineno: Some(ln(2)),
                        },
                        DiffLine {
                            kind: LineKind::Context,
                            text: "gamma".to_string(),
                            old_lineno: Some(ln(3)),
                            new_lineno: Some(ln(3)),
                        },
                        DiffLine {
                            kind: LineKind::Added,
                            text: "delta".to_string(),
                            old_lineno: None,
                            new_lineno: Some(ln(4)),
                        },
                    ],
                }],
            }],
        }
    }

    /// The before origin naming v1's after side as the source of f.txt's left
    /// side in the comparison.
    fn from_v1_before_origin() -> std::collections::HashMap<String, wiff_core::LineOrigin> {
        [(
            "f.txt".to_string(),
            wiff_core::LineOrigin {
                version: 1,
                side: Side::After,
            },
        )]
        .into_iter()
        .collect()
    }

    #[test]
    fn a_comparison_places_a_committed_after_comment_on_the_latest_line() {
        // The review's latest version is v2, with an agent comment on the
        // added delta line. Shown as the changes since v1, delta is still an
        // after-side line, so the comment sits above it; beta's replacement by
        // BETA shows the change since v1.
        let committed = CommentState {
            id: Ulid(9),
            author: Author {
                name: "opus".to_string(),
                kind: AuthorKind::Agent,
            },
            target: CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: ln(4),
                end_line: ln(4),
            },
            version: VersionNumber(2),
            anchor: None,
            body: "why delta?".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: Author {
                name: "opus".to_string(),
                kind: AuthorKind::Agent,
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
            synced_marker: None,
            number: Some(CommentNumber(1)),
            created_seq: Seq(0),
            updated_seq: Seq(0),
        };
        let review = Review::new(
            DiffView::new(theme()).unwrap(),
            comparison_diff(),
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            2,
            vec![committed],
            None,
        );
        let mut app = App::reviewing(review, 16, &theme());
        app.set_width(80);
        app.show_comparison(comparison_diff(), Some((1, from_v1_before_origin())));

        wince::assert_eq!(app.comparing_from(), Some(1));
        #[rustfmt::skip]
        wince::snapshot_display!(
            plain(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  f.txt\n",
            "@@ -1,3 +1,4 @@\n",
            "   1    1   alpha\n",
            "   2      - beta\n",
            "        2 + BETA\n",
            "   3    3   gamma\n",
            "┌ #1 opus (agent)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why delta?                                                                    │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        4 +└delta\n",
        );
    }

    #[test]
    fn authoring_on_a_comparison_before_line_anchors_against_the_reference_version() {
        // Commenting on beta -- a left-side line that is v1's after content --
        // anchors the draft against v1's after side, the version and side it
        // truly belongs to, so it rebases forward when the review advances.
        let mut review = Review::new(
            DiffView::new(theme()).unwrap(),
            comparison_diff(),
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            2,
            Vec::new(),
            None,
        );
        review.show_diff(comparison_diff(), Some((1, from_v1_before_origin())));
        review.add_comment(
            CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::Before,
                start_line: ln(2),
                end_line: ln(2),
            },
            "why beta?".to_string(),
        );

        let anchored = match review.take_drafts().into_iter().next() {
            Some(RecordBody::CommentEvent(CommentEvent {
                kind: CommentEventKind::Create(create),
                ..
            })) => (create.version, create.target, create.body),
            other => panic!("expected one drafted comment, got {other:?}"),
        };
        wince::assert_eq!(
            anchored,
            (
                VersionNumber(1),
                CommentTarget::Lines {
                    file: "f.txt".to_string(),
                    side: Side::After,
                    start_line: ln(2),
                    end_line: ln(2),
                },
                "why beta?".to_string(),
            )
        );
    }

    #[test]
    fn reloading_committed_comments_returns_the_cursor_to_the_comment() {
        // Saving commits the drafts and reloads the committed comments in place.
        // With the cursor resting on a comment box, the reload must return it to
        // that same comment rather than dropping it on the file header.
        let (_, comments) = commented_diff();
        let mut app = App::reviewing(commented_review(), 14, &theme());
        app.set_width(TEST_WIDTH);
        // Move the cursor onto the second, unresolved comment's box.
        app.update(Action::NextComment);
        app.update(Action::NextComment);
        let before_cursor = app.cursor();
        let before = dump(&app.visible(TEST_WIDTH));

        app.reload_comments(comments, None);

        wince::assert_eq!(app.cursor(), before_cursor);
        wince::assert_eq!(dump(&app.visible(TEST_WIDTH)), before);
    }

    #[test]
    fn reloading_comments_that_split_a_fold_grows_the_collapse_state() {
        // Another actor's comment inside a long unchanged run splits the fold
        // around it, so the reloaded document has more folds than the one first
        // rendered. The reload must grow the collapse state to match rather than
        // index past its end when rebuilding the view.
        let mut lines: Vec<(LineKind, String, u32)> = (1..=20)
            .map(|n| (LineKind::Context, format!("ctx{n:02}"), n))
            .collect();
        lines.push((LineKind::Added, "change!".to_string(), 21));
        let borrowed: Vec<(LineKind, &str, u32)> =
            lines.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect();
        let diff = Diff {
            files: vec![file("notes.txt", FileStatus::Modified, &borrowed)],
        };
        let review = Review::new(
            DiffView::new(theme()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            Vec::new(),
            None,
        );
        // Opening with no comments, the one change buried at the end leaves a
        // single leading fold over the whole unchanged run.
        let mut app = App::reviewing(review, 24, &theme());
        app.set_width(TEST_WIDTH);
        #[rustfmt::skip]
        wince::snapshot_display!(
            plain(&app, TEST_WIDTH),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  notes.txt\n",
            "@@ -1,21 +1,21 @@\n",
            "          ▸ [17 unchanged lines]  ctx17\n",
            "  18   18   ctx18\n",
            "  19   19   ctx19\n",
            "  20   20   ctx20\n",
            "       21 + change!\n",
        );

        // A comment far enough into the run that its kept context does not reach
        // the file top splits that one fold into a fold above and below it.
        let comments = vec![line_comment(
            1,
            ("opus", AuthorKind::Agent),
            "notes.txt",
            8,
            "why?",
            false,
        )];
        app.reload_comments(comments, None);

        #[rustfmt::skip]
        wince::snapshot_display!(
            plain(&app, TEST_WIDTH),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  notes.txt\n",
            "@@ -1,21 +1,21 @@\n",
            "          ▸ [4 unchanged lines]  ctx04\n",
            "   5    5   ctx05\n",
            "   6    6   ctx06\n",
            "   7    7   ctx07\n",
            "┌ #1 opus (agent)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why?                                  │\n",
            "└──────────┬───────────────────────────┘\n",
            "   8    8  └ctx08\n",
            "   9    9   ctx09\n",
            "  10   10   ctx10\n",
            "  11   11   ctx11\n",
            "          ▸ [6 unchanged lines]  ctx17\n",
            "  18   18   ctx18\n",
            "  19   19   ctx19\n",
            "  20   20   ctx20\n",
            "       21 + change!\n",
        );
    }
}
