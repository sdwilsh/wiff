//! A modal notice: a titled, word-wrapped message the reviewer dismisses with
//! any key.
//!
//! The status line truncates to the terminal width, dropping the actionable
//! tail of a long report. A notice instead wraps its text to a centered modal
//! and scrolls when the message is taller than the space, keeping the whole of
//! it readable. Selecting nothing, it exists only to be read and dismissed.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use wiff_diff::Rgb;

use crate::render::color;
use crate::wrap::wrap_line;

/// The colors a notice paints with, taken from the theme by the host.
#[derive(Debug, Clone, Copy)]
pub struct NoticeColors {
    /// The border and title color.
    pub border: Rgb,
    /// The background filling the modal.
    pub background: Rgb,
    /// The color of the message text.
    pub text: Rgb,
    /// The color of the dismissal hint along the bottom.
    pub hint: Rgb,
}

/// The hint shown along the bottom of a notice the reviewer dismisses.
const HINT: &str = "any key to close";

/// The hint shown along the bottom of a notice reporting work in progress, which
/// clears itself when the work finishes rather than waiting on a key.
const WORKING_HINT: &str = "please wait";

/// The widest a notice lays its text out to before wrapping, unless the space
/// it is given is narrower.
const MAX_WIDTH: usize = 60;

/// A modal message wrapped to the notice width and scrolled with the reviewer's
/// own navigation.
pub struct Notice {
    title: String,
    /// The unwrapped message text, wrapped on demand by [`wrap_to`](Self::wrap_to).
    body: String,
    /// The body wrapped to `wrapped_width`, rebuilt when that width changes.
    wrapped: Vec<Line<'static>>,
    /// The width `wrapped` was laid out to, or `None` before the first wrap. A
    /// genuine width of zero is a distinct state from never having wrapped, so
    /// the first `wrap_to` still builds `wrapped` even when the width is zero.
    wrapped_width: Option<usize>,
    /// The first visible line of the wrapped body.
    top: usize,
    /// The number of body lines the window shows, set by the host each frame.
    height: usize,
    /// The hint along the bottom border: the dismissal prompt for a notice the
    /// reviewer closes, or a wait prompt for one reporting work in progress.
    hint: &'static str,
    colors: NoticeColors,
}

impl Notice {
    /// Build a notice headed `title` showing `body`, drawn with `colors`, that
    /// the reviewer dismisses with any key.
    pub fn new(title: impl Into<String>, body: impl Into<String>, colors: NoticeColors) -> Self {
        Self::with_hint(title, body, HINT, colors)
    }

    /// Build a notice reporting work in progress: headed `title` showing `body`,
    /// drawn with `colors`, hinting to wait rather than to dismiss.
    pub fn working(
        title: impl Into<String>,
        body: impl Into<String>,
        colors: NoticeColors,
    ) -> Self {
        Self::with_hint(title, body, WORKING_HINT, colors)
    }

    /// Build a notice headed `title` showing `body`, drawn with `colors` and
    /// showing `hint` along its bottom border.
    fn with_hint(
        title: impl Into<String>,
        body: impl Into<String>,
        hint: &'static str,
        colors: NoticeColors,
    ) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            wrapped: Vec::new(),
            wrapped_width: None,
            top: 0,
            height: 0,
            hint,
            colors,
        }
    }

    /// The width the notice would like for its content: its longest line, capped
    /// at [`MAX_WIDTH`], and never narrower than the title or the hint.
    pub fn width(&self) -> usize {
        let longest = self
            .body
            .lines()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        longest
            .min(MAX_WIDTH)
            .max(self.title.chars().count())
            .max(self.hint.chars().count())
    }

    /// Wrap the body to `width`, rebuilding only when the width changed, and
    /// return the number of wrapped lines so the host can size the modal.
    pub fn wrap_to(&mut self, width: usize) -> usize {
        if self.wrapped_width != Some(width) {
            let style = Style::default()
                .fg(color(self.colors.text))
                .bg(color(self.colors.background));
            let mut wrapped = Vec::new();
            for segment in self.body.split('\n') {
                let line = Line::from(Span::styled(segment.to_string(), style));
                wrapped.extend(wrap_line(&line, width));
            }
            self.wrapped = wrapped;
            self.wrapped_width = Some(width);
            self.top = self.top.min(self.max_top());
        }
        self.wrapped.len()
    }

    /// Set how many body lines the window shows, keeping the scroll in range.
    pub fn set_height(&mut self, height: usize) {
        self.height = height;
        self.top = self.top.min(self.max_top());
    }

    /// Scroll up one line, stopping at the first.
    pub fn scroll_up(&mut self) {
        self.top = self.top.saturating_sub(1);
    }

    /// Scroll down one line, stopping so the last line stays in view.
    pub fn scroll_down(&mut self) {
        self.top = (self.top + 1).min(self.max_top());
    }

    /// Scroll up by a window's worth of lines.
    pub fn page_up(&mut self) {
        self.top = self.top.saturating_sub(self.page());
    }

    /// Scroll down by a window's worth of lines.
    pub fn page_down(&mut self) {
        self.top = (self.top + self.page()).min(self.max_top());
    }

    /// Scroll to the first line.
    pub fn to_top(&mut self) {
        self.top = 0;
    }

    /// Scroll so the last line is in view.
    pub fn to_bottom(&mut self) {
        self.top = self.max_top();
    }

    /// Returns the first visible line of the scroll window.
    pub fn top(&self) -> usize {
        self.top
    }

    /// The modal's border and title color.
    pub fn border(&self) -> Rgb {
        self.colors.border
    }

    /// The background filling the modal, for the host to paint the border row
    /// and the cells the content does not cover.
    pub fn background(&self) -> Rgb {
        self.colors.background
    }

    /// The heading for the modal's border.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The modal's content lines: the visible window of wrapped body lines, then
    /// a spacer and the dismissal hint. Each is padded to the wrapped width.
    pub fn lines(&self) -> Vec<Line<'static>> {
        let width = self.wrapped_width.unwrap_or(0);
        let background = color(self.colors.background);
        let end = (self.top + self.height).min(self.wrapped.len());
        let mut lines: Vec<Line<'static>> = self.wrapped[self.top..end]
            .iter()
            .map(|line| pad(line.clone(), width, background))
            .collect();
        lines.push(Line::from(Span::styled(
            " ".repeat(width),
            Style::default().bg(background),
        )));
        lines.push(Line::from(Span::styled(
            format!("{:<width$}", clip(self.hint, width)),
            Style::default()
                .fg(color(self.colors.hint))
                .bg(background)
                .add_modifier(Modifier::BOLD),
        )));
        lines
    }

    /// A window's worth of lines for a page move, at least one.
    fn page(&self) -> usize {
        self.height.max(1)
    }

    /// The furthest the window can scroll while keeping the last line in view.
    fn max_top(&self) -> usize {
        self.wrapped.len().saturating_sub(self.height)
    }
}

/// Extend `line` with a trailing background run so it fills `width` columns.
fn pad(mut line: Line<'static>, width: usize, background: ratatui::style::Color) -> Line<'static> {
    let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(background),
        ));
    }
    line
}

/// Clip `text` to at most `width` columns.
fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::{Notice, NoticeColors};
    use crate::render::testutil::dump;
    use wiff_diff::Rgb;

    /// The notice colors used across the tests, distinct so each shows in a dump.
    fn colors() -> NoticeColors {
        NoticeColors {
            border: Rgb {
                r: 0x11,
                g: 0x11,
                b: 0x11,
            },
            background: Rgb {
                r: 0x22,
                g: 0x22,
                b: 0x22,
            },
            text: Rgb {
                r: 0x33,
                g: 0x33,
                b: 0x33,
            },
            hint: Rgb {
                r: 0x44,
                g: 0x44,
                b: 0x44,
            },
        }
    }

    #[test]
    fn a_long_message_wraps_to_the_notice_width() {
        // A message longer than the modal wraps across several lines, each in the
        // text color, then a spacer and the dismissal hint anchored below.
        let mut notice = Notice::new(
            "Notice",
            "The quick brown fox jumps over the lazy dog and then keeps running \
             well past the edge of the modal so the text has to wrap.",
            colors(),
        );
        let width = notice.width();
        let total = notice.wrap_to(width);
        notice.set_height(total);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&notice.lines()),
            "<#333333|#222222|->The quick brown fox jumps over the lazy dog and then keeps<-|#222222|->  \n",
            "<#333333|#222222|->running well past the edge of the modal so the text has to<-|#222222|->  \n",
            "<#333333|#222222|->wrap.<-|#222222|->                                                       \n",
            "<-|#222222|->                                                            \n",
            "<#444444|#222222|b>any key to close                                            \n",
        );
    }

    #[test]
    fn a_message_taller_than_its_window_scrolls_to_the_bottom() {
        // With a window shorter than the wrapped message, scrolling to the
        // bottom brings the last body line into view above the hint.
        let mut notice = Notice::new(
            "Notice",
            "The quick brown fox jumps over the lazy dog and then keeps running \
             well past the edge of the modal so the text has to wrap.",
            colors(),
        );
        let width = notice.width();
        notice.wrap_to(width);
        notice.set_height(1);
        notice.to_bottom();
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&notice.lines()),
            "<#333333|#222222|->wrap.<-|#222222|->                                                       \n",
            "<-|#222222|->                                                            \n",
            "<#444444|#222222|b>any key to close                                            \n",
        );
    }
}
