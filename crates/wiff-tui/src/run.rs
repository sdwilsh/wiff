//! The terminal event loop that drives a review.
//!
//! [`run`] takes over the terminal, draws the [`App`], and turns each key event
//! into an action through the [`Keymap`]: navigation moves the view, and a quit
//! action ends the loop and reports how the reviewer chose to leave. It is the
//! imperative shell around the pure [`App`], [`Input`], and rendering layers;
//! the terminal is always restored, including on a draw or read error.

use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use wiff_core::record::RecordBody;

use crate::action::Action;
use crate::app::{App, Update};
use crate::event::to_key_press;
use crate::input::Input;
use crate::keymap::Keymap;

/// How the reviewer chose to leave the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Leave per the configured on-exit default, which the host resolves.
    Default,
    /// Keep the session for later resumption.
    Keep,
    /// Remove the session.
    Remove,
}

/// Run the review loop over `app`, resolving key events through `keymap`, until
/// a quit action ends it. The terminal is put into raw mode on an alternate
/// screen for the duration and restored before returning. Returns how the
/// reviewer chose to leave together with any buffered draft edits to commit.
pub fn run(app: App, keymap: Keymap) -> io::Result<(Exit, Vec<RecordBody>)> {
    let mut terminal = TerminalGuard::enter()?;
    event_loop(&mut terminal.terminal, app, keymap)
}

/// Draw the current view: the visible lines over all but the last screen row,
/// with a status line filling that last row. The app is resized to the document
/// area first so its viewport matches the space the status line leaves.
pub fn draw<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let doc_height = area.height.saturating_sub(1);
        app.set_height(doc_height as usize);
        let doc_area = Rect {
            height: doc_height,
            ..area
        };
        let status_area = Rect {
            y: area.y + doc_height,
            height: 1,
            ..area
        };
        frame.render_widget(Paragraph::new(app.visible(area.width as usize)), doc_area);
        frame.render_widget(Paragraph::new(app.status(area.width as usize)), status_area);
    })?;
    Ok(())
}

/// Draw and handle events until a quit action ends the loop.
fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
    keymap: Keymap,
) -> io::Result<(Exit, Vec<RecordBody>)> {
    let mut input = Input::new(keymap);
    loop {
        draw(terminal, &mut app)?;
        // A resize is handled by the next draw, which resizes the app to match.
        if let Event::Key(key) = event::read()?
            && let Some(press) = to_key_press(key)
            && let Some(action) = input.press(press)
            && let Update::Passed(passed) = app.update(action)
            && let Some(exit) = exit_for(passed)
        {
            return Ok((exit, app.take_drafts()));
        }
    }
}

/// The exit a passed-back action calls for, or `None` when the action is not a
/// quit and is left for a later feature to handle.
fn exit_for(action: Action) -> Option<Exit> {
    match action {
        Action::Quit => Some(Exit::Default),
        Action::QuitKeep => Some(Exit::Keep),
        Action::QuitRemove => Some(Exit::Remove),
        _ => None,
    }
}

/// A live terminal in raw mode on the alternate screen, restored on drop so the
/// user's shell is never left in raw mode even if the loop errors out.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Take over the terminal: raw mode on a fresh alternate screen.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use wiff_diff::{Diff, FileStatus, LineKind};

    use super::{Exit, draw, exit_for};
    use crate::action::Action;
    use crate::app::App;
    use crate::render::DiffView;
    use crate::render::testutil::file;
    use crate::theme::Theme;

    /// The screen after drawing `app` into a `width` x `height` test terminal,
    /// as one text row per line so placement and truncation are asserted.
    fn screen(width: u16, height: u16, mut app: App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        draw(&mut terminal, &mut app).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for row in 0..height {
            for col in 0..width {
                out.push_str(buffer[(col, row)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn draws_the_diff_over_the_screen_padding_and_truncating_to_the_area() {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Context, "let x = 1;", 1)],
            )],
        };
        let document = DiffView::new(Theme::dark()).expect("view").render(&diff);
        let app = App::new(document, 0, &Theme::dark());

        // A 30x4 screen shows the three rendered rows over the top three lines
        // and the status line filling the last, each padded to 30 columns. The
        // cursor opens centered, halfway through the three-row view.
        let expected = concat!(
            "modified  src/lib.rs          \n",
            "@@ -1,1 +1,1 @@               \n",
            "   1    1   let x = 1;        \n",
            "src/lib.rs  50%               \n",
        );
        k9::assert_equal!(screen(30, 4, app), expected.to_string());
    }

    #[test]
    fn only_quit_actions_end_the_loop() {
        k9::assert_equal!(exit_for(Action::Quit), Some(Exit::Default));
        k9::assert_equal!(exit_for(Action::QuitKeep), Some(Exit::Keep));
        k9::assert_equal!(exit_for(Action::QuitRemove), Some(Exit::Remove));
        k9::assert_equal!(exit_for(Action::AddComment), None);
    }
}
