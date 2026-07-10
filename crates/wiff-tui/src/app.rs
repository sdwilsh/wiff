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

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tui_textarea::TextArea;
use ulid::Ulid;
use wiff_core::record::{CommentTarget, RecordBody};
use wiff_diff::Rgb;

use crate::action::Action;
use crate::compose::{Compose, ComposeKind};
use crate::key::{Key, KeyPress};
use crate::render::{Document, RowKind, color};
use crate::review::Review;
use crate::theme::Theme;

/// The result of feeding an [`Action`] to the [`App`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// The app handled the action; the view may have moved.
    Handled,
    /// The action is not one the app handles; the host should act on it.
    Passed(Action),
}

/// The viewport split around the inline comment editor while composing: the
/// document lines above the editor, the editor widget itself, and the lines
/// below it, in draw order down the screen.
pub struct ComposeView<'a> {
    /// The document lines shown above the editor.
    pub above: Vec<Line<'static>>,
    /// The live editor widget, rendered where its comment will appear.
    pub editor: &'a TextArea<'static>,
    /// The document lines shown below the editor.
    pub below: Vec<Line<'static>>,
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

/// The review view over a rendered diff.
pub struct App {
    document: Document,
    /// The review being edited, present when the app can author comments. When
    /// absent the app is a read-only viewport and editing actions pass through.
    review: Option<Review>,
    /// The inline comment editor, present while authoring or revising a comment.
    compose: Option<Compose>,
    /// Whether each of the document's folds is currently collapsed.
    collapsed: Vec<bool>,
    /// Whether each comment's body is currently collapsed, keyed by its stable
    /// identity so the state survives a document rebuild after a refresh.
    comment_collapsed: HashMap<Ulid, bool>,
    /// The visible lines in order, resolved from the collapse state.
    view: Vec<ViewRow>,
    cursor: usize,
    top: usize,
    height: usize,
    /// Whether the initial cursor has been centered in the viewport, which
    /// happens once the first real height is known.
    positioned: bool,
    cursor_bg: Rgb,
    status_fg: Rgb,
    status_bg: Rgb,
    /// The border color of the inline comment editor.
    compose_border: Rgb,
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
            collapsed,
            comment_collapsed,
            view: Vec::new(),
            cursor: 0,
            top: 0,
            height,
            positioned: false,
            cursor_bg: theme.cursor_bg,
            status_fg: theme.status_fg,
            status_bg: theme.status_bg,
            compose_border: theme.comment_draft_fg,
        };
        app.rebuild_view();
        app
    }

    /// Build the view over `review`, rendering its current state and retaining
    /// it so buffered edits re-render in place. Otherwise like [`new`](App::new).
    pub fn reviewing(review: Review, height: usize, theme: &Theme) -> Self {
        let mut app = Self::new(review.document(), height, theme);
        app.review = Some(review);
        app
    }

    /// Take the pending draft records to be committed to the session log,
    /// emptying the buffer. Empty when no review is attached.
    pub fn take_drafts(&mut self) -> Vec<RecordBody> {
        self.review
            .as_mut()
            .map(Review::take_drafts)
            .unwrap_or_default()
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

    /// Handle a navigation action, or pass any other action back to the host.
    pub fn update(&mut self, action: Action) -> Update {
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
            Action::ResolveComment => return self.resolve_comment(),
            Action::DeleteComment => return self.delete_comment(),
            Action::AddComment => return self.start_add_comment(),
            Action::EditComment => return self.start_edit_comment(),
            other => return Update::Passed(other),
        }
        Update::Handled
    }

    /// Toggle the resolved state of the comment the cursor is on, buffering the
    /// change and re-rendering. Passes through when no review is being edited;
    /// does nothing when the cursor is not on a comment.
    fn resolve_comment(&mut self) -> Update {
        if self.review.is_none() {
            return Update::Passed(Action::ResolveComment);
        }
        if let Some(id) = self.comment_at_cursor() {
            if let Some(review) = self.review.as_mut() {
                review.toggle_resolved(id);
            }
            self.rerender();
            self.focus_comment(id);
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
        if let Some(id) = self.comment_at_cursor() {
            if let Some(review) = self.review.as_mut() {
                let deleted = review.toggle_deleted(id);
                self.comment_collapsed.insert(id, deleted);
            }
            self.rerender();
            self.focus_comment(id);
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
            self.compose = Some(Compose::new(
                ComposeKind::Add(target),
                anchor,
                "",
                label,
                self.compose_border,
            ));
        }
        Update::Handled
    }

    /// Open the inline editor to revise the comment the cursor is on, seeded
    /// with its current body. Passes through when no review is being edited;
    /// does nothing when the cursor is not on a comment.
    fn start_edit_comment(&mut self) -> Update {
        if self.review.is_none() {
            return Update::Passed(Action::EditComment);
        }
        if let Some(id) = self.comment_at_cursor()
            && let Some(anchor) = self
                .comment_header_row(id)
                .and_then(|row| self.view_index_of_row(row))
        {
            let body = self
                .review
                .as_ref()
                .and_then(|review| review.comment_body(id))
                .unwrap_or_default();
            self.compose = Some(Compose::new(
                ComposeKind::Edit(id),
                anchor,
                &body,
                "edit comment".to_string(),
                self.compose_border,
            ));
        }
        Update::Handled
    }

    /// The target, anchor view row, and border label for a comment authored at
    /// the cursor: a line comment on a content row, a whole-file comment on a
    /// file header, or a review comment on the summary row. `None` on a row that
    /// anchors no comment.
    fn add_target_at_cursor(&self) -> Option<(CommentTarget, usize, String)> {
        match self.kind_at(self.cursor)? {
            RowKind::ReviewSummary => Some((
                CommentTarget::Review,
                self.cursor + 1,
                "new review comment".to_string(),
            )),
            RowKind::FileHeader => {
                let path = self.document.files.get(self.cursor_file()?)?.clone();
                let label = format!("new comment  {path}");
                Some((CommentTarget::File { file: path }, self.cursor + 1, label))
            }
            RowKind::Content {
                side,
                lineno: Some(lineno),
            } => {
                let (side, lineno) = (*side, *lineno);
                let path = self.document.files.get(self.cursor_file()?)?.clone();
                let label = format!("new comment  {path}:{}", lineno.get());
                let target = CommentTarget::Lines {
                    file: path,
                    side,
                    start_line: lineno,
                    end_line: lineno,
                };
                Some((target, self.cursor, label))
            }
            _ => None,
        }
    }

    /// Whether the inline comment editor is open, so the host routes raw key
    /// presses to it rather than resolving them into actions.
    pub fn composing(&self) -> bool {
        self.compose.is_some()
    }

    /// Feed a key press to the open editor: save on ctrl-s, cancel on escape
    /// (confirming first when the body has unsaved changes), and otherwise let
    /// the editor handle the press. Does nothing when the editor is closed.
    pub fn compose_key(&mut self, press: KeyPress) {
        let Some(compose) = self.compose.as_mut() else {
            return;
        };
        if compose.confirming() {
            match press.key {
                Key::Char('y') | Key::Char('Y') => self.compose = None,
                _ => compose.resume(),
            }
            return;
        }
        if press.ctrl && press.key == Key::Char('s') {
            self.submit_compose();
        } else if press.key == Key::Escape {
            if compose.is_dirty() {
                compose.begin_confirm();
            } else {
                self.compose = None;
            }
        } else {
            compose.input(press);
        }
    }

    /// Commit the open editor's body to the review: a new comment or a revision.
    /// An empty body is discarded like a cancel. Focuses the resulting comment.
    fn submit_compose(&mut self) {
        let Some(compose) = self.compose.take() else {
            return;
        };
        let body = compose.body();
        if body.is_empty() {
            return;
        }
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let id = match compose.into_kind() {
            ComposeKind::Add(target) => review.add_comment(target, body),
            ComposeKind::Edit(id) => {
                review.edit_comment(id, body);
                id
            }
        };
        self.rerender();
        self.focus_comment(id);
    }

    /// The viewport split around the inline editor, when composing: the document
    /// lines above the editor, the editor widget, and the lines below it,
    /// together filling the viewport. The editor renders where its comment will,
    /// keeping the anchored code just below it. `None` when not composing.
    pub fn compose_view(&self) -> Option<ComposeView<'_>> {
        let compose = self.compose.as_ref()?;
        if self.height == 0 {
            return None;
        }
        let editor_height = (compose.line_count() + 2).min(self.height.max(3));
        let doc_shown = self.height.saturating_sub(editor_height);
        let anchor = compose.anchor().min(self.view.len());
        // Leave the rows already above the anchor where they sit and let the
        // editor push the anchored code and everything below it down, so opening
        // the editor does not scroll the view out from under the reviewer. Only
        // when those rows would leave no room for the editor do we scroll up.
        let above_start = self.top.max(anchor.saturating_sub(doc_shown)).min(anchor);
        let below_end = (anchor + (doc_shown - (anchor - above_start))).min(self.view.len());
        let above = (above_start..anchor).map(|i| self.line_at(i)).collect();
        let below = (anchor..below_end).map(|i| self.line_at(i)).collect();
        Some(ComposeView {
            above,
            editor: compose.editor(),
            below,
        })
    }

    /// Re-render the document from the review after a buffered edit, preserving
    /// the fold and comment collapse state and keeping the cursor in view.
    fn rerender(&mut self) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let document = review.document();
        self.reload_document(document);
    }

    /// Swap in a freshly rendered `document`, carrying the collapse state over.
    /// A comment edit leaves the diff unchanged, so the fold structure matches
    /// and its collapse state survives; each surviving comment keeps its
    /// collapse state and a new one takes its rendered default.
    fn reload_document(&mut self, document: Document) {
        if document.folds.len() != self.collapsed.len() {
            self.collapsed = vec![true; document.folds.len()];
        }
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
        self.document = document;
        self.rebuild_view();
        self.cursor = self.cursor.min(self.last_view());
        self.scroll_into_view();
    }

    /// Move the cursor to comment `id`'s header row, if it is in view.
    fn focus_comment(&mut self, id: Ulid) {
        if let Some(index) = self
            .comment_header_row(id)
            .and_then(|row| self.view_index_of_row(row))
        {
            self.move_to(index);
        }
    }

    /// The lines currently in view, with the cursor row washed in the selection
    /// color across the full `width` so the highlight fills the screen.
    pub fn visible(&self, width: usize) -> Vec<Line<'static>> {
        let end = (self.top + self.height).min(self.view.len());
        (self.top..end)
            .map(|i| {
                let line = self.line_at(i);
                if i == self.cursor {
                    wash(line, self.cursor_bg, width)
                } else {
                    line
                }
            })
            .collect()
    }

    /// The line to draw for view row `index`: the document line, or the fold's
    /// marker when it stands in for a collapsed run.
    fn line_at(&self, index: usize) -> Line<'static> {
        match self.view[index] {
            ViewRow::Row(row) => self.document.lines[row].clone(),
            ViewRow::Fold(fold) => self.document.folds[fold].marker.clone(),
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
    /// drop out behind its header, and every other row appears in order.
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
                    if !hidden[row] {
                        self.view.push(ViewRow::Row(row));
                    }
                    row += 1;
                }
            }
        }
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
    fn is_comment_collapsed(&self, id: Ulid) -> bool {
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

    /// The comment the cursor is on, whether on its header or its body.
    fn comment_at_cursor(&self) -> Option<Ulid> {
        match self.kind_at(self.cursor)? {
            RowKind::CommentHeader { id } | RowKind::CommentBody { id } => Some(*id),
            _ => None,
        }
    }

    /// The document row of comment `id`'s header line.
    fn comment_header_row(&self, id: Ulid) -> Option<usize> {
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
    /// and how far through the view it sits, filled to `width`.
    pub fn status(&self, width: usize) -> Line<'static> {
        let path = self
            .cursor_file()
            .and_then(|file| self.document.files.get(file))
            .map(String::as_str)
            .unwrap_or("");
        let percent = self.progress_percent();
        let text: String = format!("{path}  {percent}%").chars().take(width).collect();
        Line::from(Span::styled(
            format!("{text:<width$}"),
            Style::default()
                .fg(color(self.status_fg))
                .bg(color(self.status_bg)),
        ))
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

/// Return `line` with every span's background replaced by `bg`, keeping each
/// span's foreground and modifiers, then padded with blank cells to `width` so
/// the background fills the row to the edge of the screen.
fn wash(mut line: Line<'static>, bg: Rgb, width: usize) -> Line<'static> {
    for span in &mut line.spans {
        span.style = span.style.bg(color(bg));
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
    use ulid::Ulid;
    use wiff_core::record::{Author, AuthorKind, CommentTarget};
    use wiff_core::review::CommentState;
    use wiff_diff::{Diff, FileStatus, LineKind, Side};

    use super::{App, ComposeView, Update};
    use crate::action::Action;
    use crate::key::{Key, KeyPress};
    use crate::render::DiffView;
    use crate::render::testutil::{dump, file, ln};
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

    /// The save chord for the editor.
    fn save() -> KeyPress {
        KeyPress::with_modifiers(Key::Char('s'), true, false, false)
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
            DiffView::new(Theme::dark()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            Vec::new(),
        )
    }

    /// The editor body and the document lines above and below it, as a human
    /// sees them stacked down the screen.
    fn dump_compose(view: &ComposeView) -> String {
        format!(
            "{}--editor--\n{}\n--below--\n{}",
            dump(&view.above),
            view.editor.lines().join("\n"),
            dump(&view.below),
        )
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
            version: 0,
            anchor: None,
            body: body.to_string(),
            resolved,
            deleted: false,
            confidence: None,
            created_seq: 0,
            updated_seq: 0,
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
        DiffView::new(Theme::dark())
            .unwrap()
            .render_review(&diff, &comments, &[])
    }

    /// The [`commented_diff`] as an editable review, for comment-authoring tests.
    fn commented_review() -> Review {
        let (diff, comments) = commented_diff();
        Review::new(
            DiffView::new(Theme::dark()).unwrap(),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            0,
            comments,
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
        DiffView::new(Theme::dark()).unwrap().render(&diff)
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
        DiffView::new(Theme::dark()).unwrap().render(&diff)
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
        DiffView::new(Theme::dark()).unwrap().render(&diff)
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
        let mut app = App::new(document, height, &Theme::dark());
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
        let mut app = App::reviewing(review, height, &Theme::dark());
        for action in actions {
            app.update(*action);
        }
        (app.cursor(), app.top(), dump(&app.visible(TEST_WIDTH)))
    }

    #[test]
    fn opens_with_the_cursor_on_the_first_file_header() {
        let (cursor, top, visible) = after(3, &[]);
        k9::assert_equal!(cursor, 0);
        k9::assert_equal!(top, 0);
        // The first three rows are shown; the cursor row (the file header) is
        // washed with the selection background out to the full width.
        let expected = "\
<#c0c5ce|#4f5b66|b>modified  src/lib.rs<-|#4f5b66|->                    
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
";
        k9::assert_equal!(visible, expected.to_string());
    }

    #[test]
    fn setting_the_height_centers_the_initial_cursor() {
        // Applying a height of nine (as the first draw does) drops the cursor
        // onto the middle visible row with the view still anchored at the top.
        let mut app = App::new(tall_document(), 0, &Theme::dark());
        app.set_height(9);
        k9::assert_equal!(app.cursor(), 4);
        k9::assert_equal!(app.top(), 0);
        let expected = "\
<#c0c5ce|-|b>added  long.txt
<#96b5b4|-|->@@ -1,20 +1,20 @@
<#65737e|#2d3b30|->        1 + <#c0c5ce|#2d3b30|->row01
<#65737e|#2d3b30|->        2 + <#c0c5ce|#2d3b30|->row02
<#65737e|#4f5b66|->        3 + <#c0c5ce|#4f5b66|->row03<-|#4f5b66|->                       
<#65737e|#2d3b30|->        4 + <#c0c5ce|#2d3b30|->row04
<#65737e|#2d3b30|->        5 + <#c0c5ce|#2d3b30|->row05
<#65737e|#2d3b30|->        6 + <#c0c5ce|#2d3b30|->row06
<#65737e|#2d3b30|->        7 + <#c0c5ce|#2d3b30|->row07
";
        k9::assert_equal!(dump(&app.visible(TEST_WIDTH)), expected.to_string());
    }

    #[test]
    fn line_down_keeps_a_third_of_the_viewport_below_the_cursor() {
        // Twelve line-downs on a height-9 view: a third of nine is three, so the
        // cursor scrolls to sit three rows above the bottom rather than pinned to
        // the last visible line.
        let actions = [Action::LineDown; 12];
        let (cursor, top, visible) = drive(tall_document(), 9, &actions);
        k9::assert_equal!(cursor, 12);
        k9::assert_equal!(top, 7);
        let expected = "\
<#65737e|#2d3b30|->        6 + <#c0c5ce|#2d3b30|->row06
<#65737e|#2d3b30|->        7 + <#c0c5ce|#2d3b30|->row07
<#65737e|#2d3b30|->        8 + <#c0c5ce|#2d3b30|->row08
<#65737e|#2d3b30|->        9 + <#c0c5ce|#2d3b30|->row09
<#65737e|#2d3b30|->       10 + <#c0c5ce|#2d3b30|->row10
<#65737e|#4f5b66|->       11 + <#c0c5ce|#4f5b66|->row11<-|#4f5b66|->                       
<#65737e|#2d3b30|->       12 + <#c0c5ce|#2d3b30|->row12
<#65737e|#2d3b30|->       13 + <#c0c5ce|#2d3b30|->row13
<#65737e|#2d3b30|->       14 + <#c0c5ce|#2d3b30|->row14
";
        k9::assert_equal!(visible, expected.to_string());
    }

    #[test]
    fn the_status_line_names_the_cursor_file_and_progress() {
        // At the top the first file is named and progress is zero; jumping to
        // the second file names it and shows how far through the view it sits.
        let mut app = App::new(document(), 10, &Theme::dark());
        k9::assert_equal!(
            dump(&[app.status(28)]),
            "<#c0c5ce|#343d46|->src/lib.rs  0%              \n".to_string()
        );
        app.update(Action::NextFile);
        k9::assert_equal!(
            dump(&[app.status(28)]),
            "<#c0c5ce|#343d46|->notes.txt  66%              \n".to_string()
        );
    }

    #[test]
    fn next_and_prev_file_jump_between_file_headers() {
        let (cursor, top, _) = after(10, &[Action::NextFile]);
        k9::assert_equal!(cursor, 4);
        k9::assert_equal!(top, 0);

        // From the second file, prev-file returns to the first header, and a
        // further prev-file stays put since there is none before it.
        let (cursor, top, _) = after(10, &[Action::NextFile, Action::PrevFile, Action::PrevFile]);
        k9::assert_equal!(cursor, 0);
        k9::assert_equal!(top, 0);
    }

    #[test]
    fn next_hunk_lands_on_the_hunk_header_and_bottom_jumps_to_the_end() {
        let (cursor, _, _) = after(10, &[Action::NextHunk]);
        k9::assert_equal!(cursor, 1);

        let (cursor, _, _) = after(10, &[Action::Bottom]);
        // Seven rows total (a header and hunk header plus content for each
        // file); the last is the added line of the second file.
        k9::assert_equal!(cursor, 6);
    }

    #[test]
    fn space_advances_a_whole_page_like_less() {
        // On a height-3 view, each page-down slides the viewport by a full screen
        // and carries the cursor along, rather than nudging it one row.
        let (cursor, top, _) = after(3, &[Action::PageDown]);
        k9::assert_equal!(cursor, 3);
        k9::assert_equal!(top, 3);

        let (cursor, top, _) = after(3, &[Action::PageDown, Action::PageDown]);
        k9::assert_equal!(cursor, 6);
        k9::assert_equal!(top, 4);

        let (cursor, top, _) = after(3, &[Action::PageDown, Action::PageDown, Action::PageUp]);
        k9::assert_equal!(cursor, 3);
        k9::assert_equal!(top, 1);
    }

    #[test]
    fn a_non_navigation_action_is_passed_back_to_the_host() {
        let mut app = App::new(document(), 10, &Theme::dark());
        k9::assert_equal!(app.update(Action::Quit), Update::Passed(Action::Quit));
        k9::assert_equal!(app.update(Action::LineDown), Update::Handled);
    }

    #[test]
    fn long_unchanged_runs_collapse_into_fold_markers() {
        // The whole collapsed view: the headers, a leading fold, the kept
        // context and the change, and a trailing fold.
        let (cursor, top, visible) = drive(folded_document(), 12, &[]);
        k9::assert_equal!(cursor, 0);
        k9::assert_equal!(top, 0);
        let expected = "\
<#c0c5ce|#4f5b66|b>modified  notes.txt<-|#4f5b66|->                     
<#96b5b4|-|->@@ -1,17 +1,17 @@
<#8a8a8a|-|->            [5 unchanged lines]  ctx05
<#65737e|-|->   6    6   <#c0c5ce|-|->ctx06
<#65737e|-|->   7    7   <#c0c5ce|-|->ctx07
<#65737e|-|->   8    8   <#c0c5ce|-|->ctx08
<#65737e|#2d3b30|->        9 + <#c0c5ce|#2d3b30|->change!
<#65737e|-|->  10   10   <#c0c5ce|-|->ctx09
<#65737e|-|->  11   11   <#c0c5ce|-|->ctx10
<#65737e|-|->  12   12   <#c0c5ce|-|->ctx11
<#8a8a8a|-|->            [5 unchanged lines]  ctx16
";
        k9::assert_equal!(visible, expected.to_string());
    }

    #[test]
    fn expanding_a_fold_reveals_its_hidden_rows() {
        // The leading fold marker is the third view row; expand it there.
        let (cursor, top, visible) = drive(
            folded_document(),
            6,
            &[Action::LineDown, Action::LineDown, Action::ToggleFold],
        );
        k9::assert_equal!(cursor, 2);
        k9::assert_equal!(top, 0);
        let expected = "\
<#c0c5ce|-|b>modified  notes.txt
<#96b5b4|-|->@@ -1,17 +1,17 @@
<#65737e|#4f5b66|->   1    1   <#c0c5ce|#4f5b66|->ctx01<-|#4f5b66|->                       
<#65737e|-|->   2    2   <#c0c5ce|-|->ctx02
<#65737e|-|->   3    3   <#c0c5ce|-|->ctx03
<#65737e|-|->   4    4   <#c0c5ce|-|->ctx04
";
        k9::assert_equal!(visible, expected.to_string());
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
        k9::assert_equal!(cursor, 2);
        k9::assert_equal!(top, 0);
        let expected = "\
<#c0c5ce|-|b>modified  notes.txt
<#96b5b4|-|->@@ -1,17 +1,17 @@
<#8a8a8a|#4f5b66|->            [5 unchanged lines]  ctx05<-|#4f5b66|->  
<#65737e|-|->   6    6   <#c0c5ce|-|->ctx06
<#65737e|-|->   7    7   <#c0c5ce|-|->ctx07
<#65737e|-|->   8    8   <#c0c5ce|-|->ctx08
";
        k9::assert_equal!(visible, expected.to_string());
    }

    #[test]
    fn comments_open_with_the_resolved_one_collapsed_and_the_rest_expanded() {
        // The resolved comment on line 1 shows only its header; the unresolved
        // comment on line 2 shows its header and both body lines.
        let (cursor, top, visible) = drive(commented_document(), 12, &[]);
        k9::assert_equal!(cursor, 0);
        k9::assert_equal!(top, 0);
        let expected = "\
<#ebcb8b|#4f5b66|b>Review<-|#4f5b66|->                                  
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->opus (agent)<#8a8a8a|-|-> [resolved]
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|-|->            * <#8fa1b3|-|->wez (human)
<#c0c5ce|-|->              why 2?
<#c0c5ce|-|->              say more
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(visible, expected.to_string());
    }

    #[test]
    fn next_and_prev_comment_jump_between_comment_headers() {
        // Two comment headers; next-comment lands on the first then the second,
        // and prev-comment walks back, staying put once past the first.
        let (cursor, _, _) = drive(commented_document(), 12, &[Action::NextComment]);
        k9::assert_equal!(cursor, 3);
        let (cursor, _, _) = drive(
            commented_document(),
            12,
            &[Action::NextComment, Action::NextComment],
        );
        k9::assert_equal!(cursor, 5);
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
        k9::assert_equal!(cursor, 3);
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
        k9::assert_equal!(cursor, 5);
        k9::assert_equal!(top, 0);
        let expected = "\
<#ebcb8b|-|b>Review
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->opus (agent)<#8a8a8a|-|-> [resolved]
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|#4f5b66|->            * <#8fa1b3|#4f5b66|->wez (human)<-|#4f5b66|->               
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(visible, expected.to_string());

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
        k9::assert_equal!(cursor, 5);
        let expected = "\
<#ebcb8b|-|b>Review
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->opus (agent)<#8a8a8a|-|-> [resolved]
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|#4f5b66|->            * <#8fa1b3|#4f5b66|->wez (human)<-|#4f5b66|->               
<#c0c5ce|-|->              why 2?
<#c0c5ce|-|->              say more
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(visible, expected.to_string());
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
        k9::assert_equal!(cursor, 5);
        k9::assert_equal!(top, 0);
        let expected = "\
<#ebcb8b|-|b>Review
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->opus (agent)<#8a8a8a|-|-> [resolved]
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|#4f5b66|->            * <#8fa1b3|#4f5b66|->wez (human)<#a3be8c|#4f5b66|-> [draft]<#8a8a8a|#4f5b66|-> [resolved]
<#c0c5ce|-|->              why 2?
<#c0c5ce|-|->              say more
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(visible, expected.to_string());
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
        k9::assert_equal!(cursor, 5);
        k9::assert_equal!(top, 0);
        let expected = "\
<#ebcb8b|-|b>Review
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->opus (agent)<#8a8a8a|-|-> [resolved]
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|#4f5b66|->            * <#8fa1b3|#4f5b66|->wez (human)<#a3be8c|#4f5b66|-> [draft]<#8a8a8a|#4f5b66|-> [deleted]
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(visible, expected.to_string());
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
        k9::assert_equal!(cursor, 5);
        k9::assert_equal!(top, 0);
        let expected = "\
<#ebcb8b|-|b>Review
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->opus (agent)<#8a8a8a|-|-> [resolved]
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|#4f5b66|->            * <#8fa1b3|#4f5b66|->wez (human)<-|#4f5b66|->               
<#c0c5ce|-|->              why 2?
<#c0c5ce|-|->              say more
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(visible, expected.to_string());
    }

    #[test]
    fn an_editing_action_passes_through_when_no_review_is_attached() {
        // Without a review the app is a read-only viewport, so resolve and
        // delete are handed back to the host untouched.
        let mut app = App::new(commented_document(), 12, &Theme::dark());
        k9::assert_equal!(
            app.update(Action::ResolveComment),
            Update::Passed(Action::ResolveComment)
        );
        k9::assert_equal!(
            app.update(Action::DeleteComment),
            Update::Passed(Action::DeleteComment)
        );
        k9::assert_equal!(
            app.update(Action::AddComment),
            Update::Passed(Action::AddComment)
        );
        k9::assert_equal!(
            app.update(Action::EditComment),
            Update::Passed(Action::EditComment)
        );
    }

    #[test]
    fn adding_a_comment_on_a_line_renders_it_as_a_draft() {
        // Move onto the added line, author a comment there, and save it: it
        // appears as a pending draft in a block above the line it anchors.
        let mut app = App::reviewing(plain_review(), 12, &Theme::dark());
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        k9::assert_equal!(app.composing(), true);
        typed(&mut app, "why 2?");
        app.compose_key(save());
        k9::assert_equal!(app.composing(), false);
        let expected = "\
<#ebcb8b|-|b>Review
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|#4f5b66|->            * <#8fa1b3|#4f5b66|->wez (human)<#a3be8c|#4f5b66|-> [draft]<-|#4f5b66|->       
<#c0c5ce|-|->              why 2?
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump(&app.visible(TEST_WIDTH)), expected.to_string());
    }

    #[test]
    fn the_editor_renders_inline_above_the_anchored_line() {
        // With the editor open on the added line, the split places the anchored
        // line just below the editor and the seeded body sits in the editor.
        let mut app = App::reviewing(plain_review(), 8, &Theme::dark());
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        typed(&mut app, "why 2?");
        let view = app.compose_view().expect("composing");
        let expected = "\
<#ebcb8b|-|b>Review
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
--editor--
why 2?
--below--
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump_compose(&view), expected.to_string());
    }

    #[test]
    fn cancelling_a_clean_editor_closes_it_at_once() {
        // Escape with nothing typed leaves editing immediately with no draft.
        let mut app = App::reviewing(plain_review(), 12, &Theme::dark());
        app.update(Action::AddComment);
        k9::assert_equal!(app.composing(), true);
        app.compose_key(KeyPress::new(Key::Escape));
        k9::assert_equal!(app.composing(), false);
        k9::assert_equal!(app.take_drafts(), Vec::new());
    }

    #[test]
    fn cancelling_a_changed_editor_asks_before_discarding() {
        // Escape after typing does not close; it asks. Declining resumes
        // editing; escaping again and confirming discards without a draft.
        let mut app = App::reviewing(plain_review(), 12, &Theme::dark());
        app.update(Action::AddComment);
        typed(&mut app, "hmm");
        app.compose_key(KeyPress::new(Key::Escape));
        k9::assert_equal!(app.composing(), true);
        app.compose_key(ch('n'));
        k9::assert_equal!(app.composing(), true);
        app.compose_key(KeyPress::new(Key::Escape));
        app.compose_key(ch('y'));
        k9::assert_equal!(app.composing(), false);
        k9::assert_equal!(app.take_drafts(), Vec::new());
    }

    #[test]
    fn an_empty_body_is_discarded_on_save() {
        // Saving an untouched editor authors nothing.
        let mut app = App::reviewing(plain_review(), 12, &Theme::dark());
        app.update(Action::AddComment);
        app.compose_key(save());
        k9::assert_equal!(app.composing(), false);
        k9::assert_equal!(app.take_drafts(), Vec::new());
    }

    #[test]
    fn editing_a_comment_seeds_the_editor_and_rewrites_the_body() {
        // Land on the unresolved comment and edit it: the editor opens with its
        // current body, and saving a new body rewrites it, badged as a draft.
        let mut app = App::reviewing(commented_review(), 12, &Theme::dark());
        app.update(Action::NextComment);
        app.update(Action::NextComment);
        app.update(Action::EditComment);
        let view = app.compose_view().expect("composing");
        k9::assert_equal!(
            view.editor.lines().join("\n"),
            "why 2?\nsay more".to_string()
        );
        drop(view);
        // Replace the body: clear the two seeded lines, then type a new one.
        for _ in 0..20 {
            app.compose_key(KeyPress::new(Key::Backspace));
        }
        typed(&mut app, "use a constant");
        app.compose_key(save());
        k9::assert_equal!(app.composing(), false);
        let expected = "\
<#ebcb8b|-|b>Review
<#c0c5ce|-|b>modified  src/lib.rs
<#96b5b4|-|->@@ -1,2 +1,2 @@
<#8a8a8a|-|->            * <#8fa1b3|-|->opus (agent)<#8a8a8a|-|-> [resolved]
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#8a8a8a|#4f5b66|->            * <#8fa1b3|#4f5b66|->wez (human)<#a3be8c|#4f5b66|-> [draft]<-|#4f5b66|->       
<#c0c5ce|-|->              use a constant
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
";
        k9::assert_equal!(dump(&app.visible(TEST_WIDTH)), expected.to_string());
    }
}
