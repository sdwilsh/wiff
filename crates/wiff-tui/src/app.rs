//! The scrolling review view: a cursor over a rendered [`Document`].
//!
//! The app owns the rendered diff and the viewport into it: which row the
//! cursor is on and which row is at the top of the screen. It consumes the
//! navigation [`Action`]s (line, page, file, and hunk movement) and reports any
//! other action back to the host to handle. The cursor row is washed with the
//! theme's selection color so the reviewer can see where they are.

use ratatui::text::Line;
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

/// The review view over a rendered diff.
pub struct App {
    document: Document,
    cursor: usize,
    top: usize,
    height: usize,
    cursor_bg: Rgb,
}

impl App {
    /// Build the view over `document`, showing `height` rows, selecting rows
    /// with `theme`'s cursor color.
    pub fn new(document: Document, height: usize, theme: &Theme) -> Self {
        Self {
            document,
            cursor: 0,
            top: 0,
            height,
            cursor_bg: theme.cursor_bg,
        }
    }

    /// The row the cursor is on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The first visible row.
    pub fn top(&self) -> usize {
        self.top
    }

    /// Resize the viewport to `height` rows, keeping the cursor visible.
    pub fn set_height(&mut self, height: usize) {
        self.height = height;
        self.scroll_into_view();
    }

    /// Handle a navigation action, or pass any other action back to the host.
    pub fn update(&mut self, action: Action) -> Update {
        match action {
            Action::LineDown => self.move_to(self.cursor + 1),
            Action::LineUp => self.move_to(self.cursor.saturating_sub(1)),
            Action::PageDown => self.move_to(self.cursor + self.page()),
            Action::PageUp => self.move_to(self.cursor.saturating_sub(self.page())),
            Action::Top => self.move_to(0),
            Action::Bottom => self.move_to(self.last_row()),
            Action::NextFile => self.jump_forward(Landmark::File),
            Action::PrevFile => self.jump_backward(Landmark::File),
            Action::NextHunk => self.jump_forward(Landmark::Hunk),
            Action::PrevHunk => self.jump_backward(Landmark::Hunk),
            other => return Update::Passed(other),
        }
        Update::Handled
    }

    /// The lines currently in view, with the cursor row washed in the selection
    /// color so it stands out.
    pub fn visible(&self) -> Vec<Line<'static>> {
        let end = (self.top + self.height).min(self.document.lines.len());
        self.document.lines[self.top..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| {
                if self.top + offset == self.cursor {
                    wash(line, self.cursor_bg)
                } else {
                    line.clone()
                }
            })
            .collect()
    }

    /// A page's worth of rows for page up/down, at least one.
    fn page(&self) -> usize {
        self.height.max(1)
    }

    /// The last addressable row, or zero for an empty document.
    fn last_row(&self) -> usize {
        self.document.rows.len().saturating_sub(1)
    }

    /// Move the cursor to `target`, clamped to the document, then scroll to keep
    /// it in view.
    fn move_to(&mut self, target: usize) {
        self.cursor = target.min(self.last_row());
        self.scroll_into_view();
    }

    /// Move the cursor to the next `landmark` row after it, if any.
    fn jump_forward(&mut self, landmark: Landmark) {
        if let Some(row) = (self.cursor + 1..self.document.rows.len())
            .find(|&i| landmark.matches(&self.document.rows[i].kind))
        {
            self.move_to(row);
        }
    }

    /// Move the cursor to the nearest `landmark` row before it, if any.
    fn jump_backward(&mut self, landmark: Landmark) {
        if let Some(row) = (0..self.cursor)
            .rev()
            .find(|&i| landmark.matches(&self.document.rows[i].kind))
        {
            self.move_to(row);
        }
    }

    /// Slide the viewport so the cursor row is visible.
    fn scroll_into_view(&mut self) {
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.height > 0 && self.cursor >= self.top + self.height {
            self.top = self.cursor + 1 - self.height;
        }
    }
}

/// Clone `line` with every span's background replaced by `bg`, keeping each
/// span's foreground and modifiers.
fn wash(line: &Line<'static>, bg: Rgb) -> Line<'static> {
    let mut washed = line.clone();
    for span in &mut washed.spans {
        span.style = span.style.bg(color(bg));
    }
    washed
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

    /// Drive `actions` through a fresh app and return its cursor, top, and the
    /// dumped visible lines.
    fn after(height: usize, actions: &[Action]) -> (usize, usize, String) {
        let mut app = App::new(document(), height, &Theme::dark());
        for action in actions {
            app.update(*action);
        }
        (app.cursor(), app.top(), dump(&app.visible()))
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
    fn line_down_moves_the_cursor_and_scrolls_when_it_reaches_the_bottom() {
        // Four line-downs on a height-3 view: the cursor reaches row 4, so the
        // viewport has scrolled to show rows 2..5.
        let (cursor, top, visible) = after(
            3,
            &[
                Action::LineDown,
                Action::LineDown,
                Action::LineDown,
                Action::LineDown,
            ],
        );
        k9::assert_equal!(cursor, 4);
        k9::assert_equal!(top, 2);
        let expected = "\
<#65737e|-|->   1    1   <#b48ead|-|->let<#c0c5ce|-|-> x <#c0c5ce|-|->=<#c0c5ce|-|-> <#d08770|-|->1<#c0c5ce|-|->;
<#65737e|#2d3b30|->        2 + <#b48ead|#2d3b30|->let<#c0c5ce|#2d3b30|-> y <#c0c5ce|#2d3b30|->=<#c0c5ce|#2d3b30|-> <#d08770|#2d3b30|->2<#c0c5ce|#2d3b30|->;
<#c0c5ce|#4f5b66|b>added  notes.txt
";
        k9::assert_equal!(visible, expected.to_string());
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
    fn a_non_navigation_action_is_passed_back_to_the_host() {
        let mut app = App::new(document(), 10, &Theme::dark());
        k9::assert_equal!(app.update(Action::Quit), Update::Passed(Action::Quit));
        k9::assert_equal!(app.update(Action::LineDown), Update::Handled);
    }
}
