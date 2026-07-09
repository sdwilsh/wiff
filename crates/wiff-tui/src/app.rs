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

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use wiff_diff::Rgb;

use crate::action::Action;
use crate::render::{Document, RowKind, color};
use crate::theme::Theme;

/// The result of feeding an [`Action`] to the [`App`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// The app handled the action; the view may have moved.
    Handled,
    /// The action is not one the app handles; the host should act on it.
    Passed(Action),
}

/// Which family of rows a jump seeks: file headers or hunk headers.
#[derive(Debug, Clone, Copy)]
enum Landmark {
    File,
    Hunk,
}

impl Landmark {
    /// Whether `kind` is a row this landmark jumps between.
    fn matches(self, kind: &RowKind) -> bool {
        matches!(
            (self, kind),
            (Landmark::File, RowKind::FileHeader) | (Landmark::Hunk, RowKind::HunkHeader { .. })
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
    /// Whether each of the document's folds is currently collapsed.
    collapsed: Vec<bool>,
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
}

impl App {
    /// Build the view over `document`, showing `height` rows, selecting rows
    /// with `theme`'s cursor color. Every fold starts collapsed.
    pub fn new(document: Document, height: usize, theme: &Theme) -> Self {
        let collapsed = vec![true; document.folds.len()];
        let mut app = Self {
            document,
            collapsed,
            view: Vec::new(),
            cursor: 0,
            top: 0,
            height,
            positioned: false,
            cursor_bg: theme.cursor_bg,
            status_fg: theme.status_fg,
            status_bg: theme.status_bg,
        };
        app.rebuild_view();
        app
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
            Action::ToggleFold => self.toggle_fold(),
            other => return Update::Passed(other),
        }
        Update::Handled
    }

    /// The lines currently in view, with the cursor row washed in the selection
    /// color so it stands out.
    pub fn visible(&self) -> Vec<Line<'static>> {
        let end = (self.top + self.height).min(self.view.len());
        (self.top..end)
            .map(|i| {
                let line = self.line_at(i);
                if i == self.cursor {
                    wash(line, self.cursor_bg)
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
    /// marker in place of the rows it hides; every other row appears in order.
    fn rebuild_view(&mut self) {
        self.view.clear();
        let mut row = 0;
        while row < self.document.rows.len() {
            match self.fold_starting_at(row) {
                Some(fold) if self.collapsed[fold] => {
                    self.view.push(ViewRow::Fold(fold));
                    row = self.document.folds[fold].end;
                }
                _ => {
                    self.view.push(ViewRow::Row(row));
                    row += 1;
                }
            }
        }
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
/// span's foreground and modifiers.
fn wash(mut line: Line<'static>, bg: Rgb) -> Line<'static> {
    for span in &mut line.spans {
        span.style = span.style.bg(color(bg));
    }
    line
}

#[cfg(test)]
mod tests {
    use wiff_diff::{Diff, FileStatus, LineKind};

    use super::{App, Update};
    use crate::action::Action;
    use crate::render::DiffView;
    use crate::render::testutil::{dump, file};
    use crate::theme::Theme;

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
        (app.cursor(), app.top(), dump(&app.visible()))
    }

    /// Drive `actions` over the two-file [`document`].
    fn after(height: usize, actions: &[Action]) -> (usize, usize, String) {
        drive(document(), height, actions)
    }

    #[test]
    fn opens_with_the_cursor_on_the_first_file_header() {
        let (cursor, top, visible) = after(3, &[]);
        k9::assert_equal!(cursor, 0);
        k9::assert_equal!(top, 0);
        // The first three rows are shown; the cursor row (the file header) is
        // washed with the selection background.
        let expected = "\
<#c0c5ce|#4f5b66|b>modified  src/lib.rs
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
<#65737e|#4f5b66|->        3 + <#c0c5ce|#4f5b66|->row03
<#65737e|#2d3b30|->        4 + <#c0c5ce|#2d3b30|->row04
<#65737e|#2d3b30|->        5 + <#c0c5ce|#2d3b30|->row05
<#65737e|#2d3b30|->        6 + <#c0c5ce|#2d3b30|->row06
<#65737e|#2d3b30|->        7 + <#c0c5ce|#2d3b30|->row07
";
        k9::assert_equal!(dump(&app.visible()), expected.to_string());
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
<#65737e|#4f5b66|->       11 + <#c0c5ce|#4f5b66|->row11
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
<#c0c5ce|#4f5b66|b>modified  notes.txt
<#96b5b4|-|->@@ -1,17 +1,17 @@
<#8a8a8a|-|->            [5 unchanged lines]
<#65737e|-|->   6    6   <#c0c5ce|-|->ctx06
<#65737e|-|->   7    7   <#c0c5ce|-|->ctx07
<#65737e|-|->   8    8   <#c0c5ce|-|->ctx08
<#65737e|#2d3b30|->        9 + <#c0c5ce|#2d3b30|->change!
<#65737e|-|->  10   10   <#c0c5ce|-|->ctx09
<#65737e|-|->  11   11   <#c0c5ce|-|->ctx10
<#65737e|-|->  12   12   <#c0c5ce|-|->ctx11
<#8a8a8a|-|->            [5 unchanged lines]
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
<#65737e|#4f5b66|->   1    1   <#c0c5ce|#4f5b66|->ctx01
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
<#8a8a8a|#4f5b66|->            [5 unchanged lines]
<#65737e|-|->   6    6   <#c0c5ce|-|->ctx06
<#65737e|-|->   7    7   <#c0c5ce|-|->ctx07
<#65737e|-|->   8    8   <#c0c5ce|-|->ctx08
";
        k9::assert_equal!(visible, expected.to_string());
    }
}
