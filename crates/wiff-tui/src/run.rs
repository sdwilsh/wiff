//! The terminal event loop that drives a review.
//!
//! [`run`] takes over the terminal, draws the [`App`], and turns each key event
//! into an action through the [`Keymap`]: navigation moves the view, and a quit
//! action ends the loop and reports how the reviewer chose to leave. It is the
//! imperative shell around the pure [`App`], [`Input`], and rendering layers;
//! the terminal is always restored, including on a draw or read error.

use std::io::{self, Stdout};
use std::time::Duration;

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use wiff_core::record::RecordBody;

use crate::action::Action;
use crate::app::{App, ComposeView, Update};
use crate::event::to_key_press;
use crate::exit::{Exit, ExitDialog};
use crate::input::Input;
use crate::key::Key;
use crate::keymap::Keymap;
use crate::picker::Picker;
use crate::render::color;

/// How long the loop waits for a key before waking to pick up another actor's
/// changes to the session. Long enough that an idle review costs almost nothing,
/// short enough that an update lands promptly.
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Run the review loop over `app`, resolving key events through `keymap`, until
/// a quit action ends it. A [`Action::Refresh`] is handed to `refresh`, which
/// recaptures the diff and reloads the app in place, and a [`Action::Save`] to
/// `save`, which commits the pending drafts and keeps the review open; both
/// report their own outcome through the app's status line. While the review sits
/// idle the loop periodically calls `sync`, which picks up another actor's
/// updates to the session and folds them into the app, returning whether it
/// changed anything so the loop repaints only when it did. The terminal is put
/// into raw mode on an alternate screen for the duration and restored before
/// returning. Returns how the reviewer chose to leave together with any buffered
/// draft edits still to commit.
pub fn run(
    app: App,
    keymap: Keymap,
    refresh: impl FnMut(&mut App),
    save: impl FnMut(&mut App),
    sync: impl FnMut(&mut App) -> bool,
) -> io::Result<(Exit, Vec<RecordBody>)> {
    let mut terminal = TerminalGuard::enter()?;
    event_loop(&mut terminal.terminal, app, keymap, refresh, save, sync)
}

/// Draw the current view: the visible lines over all but the last screen row,
/// with a status line filling that last row. The app is resized to the document
/// area first so its viewport matches the space the status line leaves.
pub fn draw<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let doc_height = area.height.saturating_sub(1);
        app.set_width(area.width as usize);
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
        if let Some(compose) = app.compose_view(area.width as usize) {
            render_compose(frame, doc_area, compose);
        } else {
            frame.render_widget(Paragraph::new(app.visible(area.width as usize)), doc_area);
        }
        frame.render_widget(Paragraph::new(app.status(area.width as usize)), status_area);
        // The exit dialog and the modal list each float centered over whatever
        // they interrupt.
        if let Some(dialog) = app.exit_dialog() {
            render_exit_dialog(frame, doc_area, dialog);
        }
        if app.picking() {
            render_picker(frame, doc_area, app);
        }
    })?;
    Ok(())
}

/// Draw the exit dialog centered over `area`, clearing the cells behind it so
/// the underlying view does not show through the box.
fn render_exit_dialog(frame: &mut Frame, area: Rect, dialog: &ExitDialog) {
    let width = (dialog.width() as u16 + 2).min(area.width);
    let height = (dialog.height() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color(dialog.border())))
        .title(dialog.title().to_string());
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(dialog.lines()).block(block), rect);
}

/// Draw the modal list centered over `area`, clearing the cells behind it. The
/// list's window is first sized to the space left inside the border, spacer, and
/// hint, so a list taller than the screen scrolls rather than overflowing.
fn render_picker(frame: &mut Frame, area: Rect, app: &mut App) {
    // The border takes two rows and the spacer and hint one each; the rest is
    // the room the list has to show its rows.
    let chrome = 4u16;
    let visible = app
        .picker()
        .map(Picker::list_len)
        .unwrap_or(0)
        .min(area.height.saturating_sub(chrome) as usize);
    app.picker_set_height(visible);
    let Some(picker) = app.picker() else {
        return;
    };
    let inner = picker.width().min(area.width.saturating_sub(2) as usize);
    let width = (inner as u16 + 2).min(area.width);
    let height = (visible as u16 + chrome).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color(picker.border())))
        .title(picker.title().to_string());
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(picker.lines(inner)).block(block), rect);
}

/// Draw the inline comment editor into `area`: the document lines above it, the
/// editor box, and the lines below, stacked to fill the document area.
fn render_compose(frame: &mut Frame, area: Rect, view: ComposeView) {
    let above_height = view.above.len() as u16;
    let below_height = view.below.len() as u16;
    let editor_height = area.height.saturating_sub(above_height + below_height);
    let above_area = Rect {
        height: above_height,
        ..area
    };
    let editor_area = Rect {
        y: area.y + above_height,
        height: editor_height,
        ..area
    };
    let below_area = Rect {
        y: area.y + above_height + editor_height,
        height: below_height,
        ..area
    };
    frame.render_widget(Paragraph::new(view.above), above_area);
    frame.render_widget(view.editor, editor_area);
    frame.render_widget(Paragraph::new(view.below), below_area);
}

/// Draw and handle events until a quit action ends the loop.
fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
    keymap: Keymap,
    mut refresh: impl FnMut(&mut App),
    mut save: impl FnMut(&mut App),
    mut sync: impl FnMut(&mut App) -> bool,
) -> io::Result<(Exit, Vec<RecordBody>)> {
    let mut input = Input::new(keymap);
    // Repaint only when the view might have changed, so an idle poll that finds
    // no session update does not redraw. Set the first time through to paint the
    // opening frame.
    let mut dirty = true;
    loop {
        if dirty {
            draw(terminal, &mut app)?;
            dirty = false;
        }
        // A wait bounded by the poll interval, so a lull between key presses
        // wakes the loop to fold in another actor's session updates. A pending
        // key press is delivered at once, so responsiveness is unaffected.
        if !event::poll(POLL_INTERVAL)? {
            // Only pick up updates in the plain review view: a comment edit,
            // search prompt, or exit dialog owns a spot or buffer that a reload
            // would disturb, so the change waits until the reviewer returns.
            if !app.composing()
                && !app.searching()
                && !app.exiting()
                && !app.picking()
                && sync(&mut app)
            {
                dirty = true;
            }
            continue;
        }
        // Any event may move the view or resize the terminal, so repaint once it
        // is handled. A resize is picked up by that next draw.
        dirty = true;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        let Some(press) = to_key_press(key) else {
            continue;
        };
        // While the search prompt, the inline editor, or the exit dialog is
        // open, raw presses go to it rather than being resolved into review
        // actions.
        if app.searching() {
            app.search_key(press);
        } else if app.composing() {
            app.compose_key(press);
        } else if app.exiting() {
            app.exit_key(press);
        } else if app.picking() {
            // Enter activates the highlight and escape closes the list; every
            // other press resolves through the keymap so the list moves with the
            // reviewer's own navigation bindings.
            match press.key {
                Key::Enter => app.picker_activate(),
                Key::Escape => app.picker_cancel(),
                _ => {
                    if let Some(action) = input.press(press) {
                        app.picker_nav(action);
                    }
                }
            }
        } else if let Some(action) = input.press(press) {
            // Refresh and save are the passed-back actions the loop acts on
            // itself, handing each to the host: refresh recaptures and reloads
            // the app in place, save commits the pending drafts.
            match app.update(action) {
                Update::Passed(Action::Refresh) => refresh(&mut app),
                Update::Passed(Action::Save) => save(&mut app),
                _ => {}
            }
        }
        // A quit action or a confirmed dialog choice settles how to leave.
        if let Some(exit) = app.pending_exit() {
            return Ok((exit, app.take_drafts()));
        }
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

    use wiff_core::record::{Author, AuthorKind};

    use super::draw;
    use crate::action::Action;
    use crate::app::App;
    use crate::exit::ExitDefault;
    use crate::key::{Key, KeyPress};
    use crate::render::DiffView;
    use crate::render::testutil::file;
    use crate::review::Review;
    use crate::theme::Theme;

    /// A review over a one-line added file authored by a human, for driving the
    /// exit dialog after a draft is made.
    fn draft_review() -> Review {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Added, "let y = 2;", 1)],
            )],
        };
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        Review::new(
            DiffView::new(Theme::dark()).expect("view"),
            diff,
            author,
            0,
            Vec::new(),
        )
    }

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
            "src/lib.rs         0 open  50%\n",
        );
        k9::assert_equal!(screen(30, 4, app), expected.to_string());
    }

    #[test]
    fn draws_the_inline_editor_box_above_the_anchored_line() {
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
        let view = DiffView::new(Theme::dark()).expect("view");
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let mut app = App::reviewing(
            Review::new(view, diff, author, 0, Vec::new()),
            0,
            &Theme::dark(),
        );
        // Move onto the added line and open the editor there, then type a body.
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for c in "why 2?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }

        // The editor renders as a bordered box titled with the target and the
        // save/cancel hint, sitting just above the added line it anchors.
        let expected = concat!(
            "Review [press c here to draft \n",
            "modified  src/lib.rs          \n",
            "@@ -1,2 +1,2 @@               \n",
            "   1    1   let x = 1;        \n",
            "┌ new comment  src/lib.rs:2  ┐\n",
            "│why 2?                      │\n",
            "└────────────────────────────┘\n",
            "        2 + let y = 2;        \n",
            "src/lib.rs        0 open  100%\n",
        );
        k9::assert_equal!(screen(30, 9, app), expected.to_string());
    }

    #[test]
    fn draws_the_exit_dialog_centered_over_the_view_with_pending_drafts() {
        let mut app = App::reviewing(draft_review(), 0, &Theme::dark())
            .with_exit_default(ExitDefault::Prompt);
        // Author a comment so a draft is pending, then quit to raise the dialog.
        app.update(Action::AddComment);
        for c in "why?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        app.compose_key(KeyPress::with_modifiers(Key::Char('d'), true, false, false));
        app.update(Action::Quit);

        // The three-choice dialog floats centered over the review, the first
        // choice highlighted with its marker and the key hint along the bottom.
        // It clears the cells behind it, so the underlying diff shows only where
        // the box does not cover it.
        let expected = concat!(
            "Review [press c here to draft the review comment] \n",
            "┌ w┌You have uncommitted comments─────────────┐sol\n",
            "│wh│> Commit review                           │  │\n",
            "└──│  Quit without saving                     │──┘\n",
            "mod│  Remove session                          │   \n",
            "@@ │                                          │   \n",
            "   │  up/down move   enter select   esc cancel│   \n",
            "   └──────────────────────────────────────────┘   \n",
            "                                                  \n",
            "                                                  \n",
            "src/lib.rs                           * 1 open  83%\n",
        );
        k9::assert_equal!(screen(50, 11, app), expected.to_string());
    }
}
