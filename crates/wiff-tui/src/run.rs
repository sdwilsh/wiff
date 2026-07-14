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
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Margin, Rect};
use ratatui::style::Style;
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::{Frame, Terminal};

use wiff_core::record::RecordBody;
use wiff_diff::Rgb;

use crate::action::Action;
use crate::app::{App, CompareRequest, ComposeColumn, ComposeView, FloatView, Update};
use crate::event::to_key_press;
use crate::exit::Exit;
use crate::input::Input;
use crate::key::Key;
use crate::keymap::Keymap;
use crate::render::{COLUMN_DIVIDER, color};

/// How long the loop waits for a key before waking to pick up another actor's
/// changes to the session. Long enough that an idle review costs almost nothing,
/// short enough that an update arrives promptly.
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// The poll interval used while background highlighting is in progress, shorter
/// than [`POLL_INTERVAL`] so arriving parses reveal without waiting a full poll
/// cycle.
const HIGHLIGHT_POLL_INTERVAL: Duration = Duration::from_millis(30);

/// Run the review loop over `app`, resolving key events through `keymap`, until
/// a quit action ends it. A [`Action::Refresh`] is handed to `refresh`, which
/// recaptures the diff and reloads the app in place, and a [`Action::Save`] to
/// `save`, which commits the pending drafts and keeps the review open; both
/// report their own outcome through the app's status line. When the reviewer
/// chooses a version to compare against, `compare` reconstructs that diff and
/// shows it in place. After each key and whenever the review sits idle the loop
/// calls `sync`, which picks up another actor's updates to the session and folds
/// them into the app, returning whether it changed anything so the loop repaints
/// only when it did. The terminal is put into raw mode on an alternate screen for the
/// duration and restored before returning. Returns how the reviewer chose to
/// leave together with any buffered draft edits still to commit.
pub fn run(
    app: App,
    keymap: Keymap,
    refresh: impl FnMut(&mut App),
    save: impl FnMut(&mut App),
    sync: impl FnMut(&mut App) -> bool,
    compare: impl FnMut(&mut App, CompareRequest),
) -> io::Result<(Exit, Vec<RecordBody>)> {
    let mut terminal = TerminalGuard::enter()?;
    event_loop(
        &mut terminal.terminal,
        app,
        keymap,
        refresh,
        save,
        sync,
        compare,
    )
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
        // Fill the document area with the theme background so a light or dark
        // theme reads coherently over the terminal's own background, then draw
        // the view over it; rows and gaps without their own fill keep this.
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(crate::render::color(app.background()))),
            doc_area,
        );
        if let Some(compose) = app.compose_view(area.width as usize) {
            render_compose(frame, doc_area, compose);
        } else {
            frame.render_widget(Paragraph::new(app.visible(area.width as usize)), doc_area);
            // A detached editor floats over the ordinary document at a screen
            // edge while the cursor roams the diff behind it.
            if let Some(float) = app.compose_float(area.width as usize) {
                render_float(frame, doc_area, float);
            }
        }
        frame.render_widget(Paragraph::new(app.status(area.width as usize)), status_area);
        // The modal list floats centered over whatever it interrupts.
        if app.picking() {
            render_picker(frame, doc_area, app);
        }
        // The help overlay floats centered like the modal list.
        if app.helping() {
            render_help(frame, doc_area, app);
        }
    })?;
    Ok(())
}

/// The centered rectangle, content width, and visible row count for a scrolling
/// modal overlay of `total` rows and `content_width` columns floating over
/// `area`. The border, spacer, and hint take four rows; the rest is the room the
/// rows have, so an overlay taller than that scrolls rather than overflowing.
fn centered_modal(area: Rect, total: usize, content_width: usize) -> (Rect, usize, usize) {
    let chrome = 4u16;
    let visible = total.min(area.height.saturating_sub(chrome) as usize);
    let inner = content_width.min(area.width.saturating_sub(2) as usize);
    let width = (inner as u16 + 2).min(area.width);
    let height = (visible as u16 + chrome).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    (rect, inner, visible)
}

/// Draw the modal list centered over `area`, clearing the cells behind it, its
/// window sized so a list taller than the space scrolls rather than overflowing.
fn render_picker(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some((total, content_width)) = app.picker().map(|p| (p.list_len(), p.width())) else {
        return;
    };
    let (rect, inner, visible) = centered_modal(area, total, content_width);
    app.picker_set_height(visible);
    let Some(picker) = app.picker() else {
        return;
    };
    let background = Style::default().bg(color(picker.background()));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(background.fg(color(picker.border())))
        .style(background)
        .title(picker.title().to_string());
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(picker.lines(inner)).block(block), rect);
}

/// Draw the help overlay centered over `area`, clearing the cells behind it, its
/// window sized so a reference taller than the space scrolls rather than
/// overflowing.
fn render_help(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some((total, content_width)) = app.help().map(|h| (h.list_len(), h.width())) else {
        return;
    };
    let (rect, inner, visible) = centered_modal(area, total, content_width);
    app.help_set_height(visible);
    let Some(help) = app.help() else {
        return;
    };
    let top = help.top();
    let background = Style::default().bg(color(help.background()));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(background.fg(color(help.border())))
        .style(background)
        .title(help.title().to_string());
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(help.lines(inner)).block(block), rect);
    // More rows than the window shows draws a scrollbar down the right border,
    // spanning the content rows above the spacer and hint, so the reviewer can
    // see there is more of the reference off-screen.
    if total > visible {
        let mut state = ScrollbarState::new(total.saturating_sub(visible) + 1)
            .position(top)
            .viewport_content_length(visible);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let track = Rect {
            y: rect.y + 1,
            height: visible as u16,
            ..rect
        };
        frame.render_stateful_widget(scrollbar, track, &mut state);
    }
}

/// Draw the inline comment editor into `area`: the document lines above it, the
/// editor box, and the lines below, stacked to fill the document area.
fn render_compose(frame: &mut Frame, area: Rect, view: ComposeView) {
    let above_height = view.above.len() as u16;
    // Size the box to exactly its shown rows plus the border. On a short
    // document the leftover rows fall to the area below the editor rather than
    // padding the box, so the editor never grows past what it is showing.
    let editor_height =
        (view.editor_rows.len() as u16 + 2).min(area.height.saturating_sub(above_height));
    let above_area = Rect {
        height: above_height,
        ..area
    };
    let editor_area = Rect {
        y: area.y + above_height,
        height: editor_height,
        ..area
    };
    let below_y = area.y + above_height + editor_height;
    let below_area = Rect {
        y: below_y,
        height: area.bottom().saturating_sub(below_y),
        ..area
    };
    frame.render_widget(Paragraph::new(view.above), above_area);
    frame.render_widget(Paragraph::new(view.below), below_area);
    // In a side-by-side layout the editor stands in its column; fill the band
    // beside it and shrink the box to that column.
    let editor_area = match view.column {
        Some(column) => scope_editor_band(frame, editor_area, &column),
        None => editor_area,
    };
    draw_editor_box(
        frame,
        editor_area,
        view.editor_block,
        view.editor_rows,
        view.editor_cursor,
        view.editor_scroll,
    );
}

/// Fill the editor band beside its column and run the divider rule down it,
/// returning the box's own Rect within the column. `band` is the full-width span
/// the editor occupies vertically; the opposite column reads blank and the rule
/// aligns with the divider in the document rows above and below.
fn scope_editor_band(frame: &mut Frame, band: Rect, column: &ComposeColumn) -> Rect {
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(color(column.background))),
        band,
    );
    let divider_x = band.x + column.divider;
    if divider_x < band.right() {
        let rule = std::iter::repeat_n(COLUMN_DIVIDER, band.height as usize)
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(
            Paragraph::new(rule).style(Style::default().fg(color(column.divider_fg))),
            Rect {
                x: divider_x,
                width: 1,
                ..band
            },
        );
    }
    Rect {
        x: band.x + column.x,
        width: column.width,
        ..band
    }
}

/// Draw the detached editor floating over the diff on the row it tracks the
/// anchor to, sized to its rows and clearing the cells behind it.
fn render_float(frame: &mut Frame, area: Rect, view: FloatView) {
    let height = (view.editor_rows.len() as u16 + 2).min(area.height);
    // The row is clamped to fit against the source, but clamp again to the drawn
    // area's own height as a guard.
    let y = area.y + view.top_row.min(area.height.saturating_sub(height));
    let editor_area = Rect { y, height, ..area };
    let (cursor_bg, cursor_offset, inset) = (view.cursor_bg, view.cursor_offset, view.inset);
    // Clear resets the box to the terminal's own background, so refill it with
    // the theme background before drawing the border and rows. The full width is
    // cleared, leaving the inset margins blank rather than showing the diff the
    // box overlaps.
    frame.render_widget(Clear, editor_area);
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(color(view.background))),
        editor_area,
    );
    let box_area = Rect {
        x: editor_area.x + inset,
        width: editor_area.width.saturating_sub(2 * inset),
        ..editor_area
    };
    draw_editor_box(
        frame,
        box_area,
        view.editor_block,
        view.editor_rows,
        view.editor_cursor,
        view.editor_scroll,
    );
    // The reviewer's cursor line, covered by the box while it roams the diff
    // behind it, shows through as the cursor tint on the chrome of the row it
    // sits behind.
    if let Some(offset) = cursor_offset {
        tint_cursor_row(frame, editor_area, box_area, offset, cursor_bg);
    }
}

/// Tint the editor chrome on the box row the roaming diff cursor sits behind:
/// the whole cursor line on the top or bottom border row, or the border cell
/// and its inset margin cells on an interior row, so the covered cursor line
/// reads around the editor.
fn tint_cursor_row(frame: &mut Frame, area: Rect, box_area: Rect, offset: u16, cursor_bg: Rgb) {
    if area.width == 0 || area.height == 0 || box_area.width == 0 {
        return;
    }
    let y = area.y + offset.min(area.height - 1);
    let bg = color(cursor_bg);
    let buffer = frame.buffer_mut();
    if offset == 0 || offset + 1 >= area.height {
        for x in area.left()..area.right() {
            buffer[(x, y)].set_bg(bg);
        }
    } else {
        // The left border and its margin, then the right border and its margin.
        // The early return on a zero-width box keeps `box_area.right()` at least
        // one, so the right-border column below never underflows.
        for x in (area.left()..=box_area.left()).chain(box_area.right() - 1..area.right()) {
            buffer[(x, y)].set_bg(bg);
        }
    }
}

/// Draw an editor box into `editor_area`: its border block and wrapped rows, a
/// scrollbar down the right border when the body overflows, and the terminal's
/// hardware cursor placed inside the border when the cursor is in the box.
fn draw_editor_box(
    frame: &mut Frame,
    editor_area: Rect,
    block: Block<'static>,
    rows: Vec<ratatui::text::Line<'static>>,
    cursor: Option<(u16, u16)>,
    scroll: Option<crate::compose::Scroll>,
) {
    frame.render_widget(Paragraph::new(rows).block(block), editor_area);
    // A body taller than the box draws a scrollbar down the right border so the
    // reviewer can see how much is off-screen.
    if let Some(scroll) = scroll {
        let visible = editor_area.height.saturating_sub(2) as usize;
        // ratatui sizes the thumb from the scrollable range, not the total, so
        // pass total - visible (the number of scroll steps) as the content
        // length. The extra step keeps the thumb just short of the full track
        // when a single row is off-screen instead of filling it outright.
        let mut state = ScrollbarState::new(scroll.total.saturating_sub(visible) + 1)
            .position(scroll.offset)
            .viewport_content_length(visible);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(
            scrollbar,
            editor_area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
    // Place the terminal's hardware cursor inside the border, past the box's
    // top and left edge, keeping an input method's candidate window on the real
    // edit point.
    if let Some((col, row)) = cursor {
        let x = editor_area.x + 1 + col;
        let y = editor_area.y + 1 + row;
        if x < editor_area.right() && y < editor_area.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

/// Draw and handle events until a quit action ends the loop.
fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
    keymap: Keymap,
    mut refresh: impl FnMut(&mut App),
    mut save: impl FnMut(&mut App),
    mut sync: impl FnMut(&mut App) -> bool,
    mut compare: impl FnMut(&mut App, CompareRequest),
) -> io::Result<(Exit, Vec<RecordBody>)> {
    let mut input = Input::new(keymap);
    // Repaint only when the view might have changed, so an idle poll that finds
    // no session update does not redraw. Set the first time through to paint the
    // opening frame.
    let mut dirty = true;
    loop {
        // A modal view -- the comment editor, search prompt, or picker (the
        // file, comment, theme, or exit list) -- owns a spot or buffer that
        // folding a document change in would disturb.
        let modal = app.composing() || app.searching() || app.picking() || app.helping();
        // Reveal files as their background highlight arrives, repainting
        // immediately only when the change is on screen.
        if !modal && app.poll_highlights() {
            dirty = true;
        }
        if dirty {
            draw(terminal, &mut app)?;
            dirty = false;
        }
        // A wait bounded by the poll interval between key presses, or
        // HIGHLIGHT_POLL_INTERVAL while background highlighting runs.
        let timeout = if app.highlighting() && !modal {
            HIGHLIGHT_POLL_INTERVAL
        } else {
            POLL_INTERVAL
        };
        if !event::poll(timeout)? {
            if !modal && sync(&mut app) {
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
        // While the search prompt, the inline editor, or a picker is open, raw
        // presses go to it rather than being resolved into review actions.
        if app.searching() {
            app.search_key(press);
        } else if app.composing() {
            app.compose_key(press);
        } else if app.helping() {
            // The overlay is a reference to read: navigation scrolls it and
            // every other key closes it. A press that only begins a multi-press
            // chord leaves it open until the chord resolves or is abandoned.
            match input.press(press) {
                Some(
                    action @ (Action::LineDown
                    | Action::LineUp
                    | Action::PageDown
                    | Action::PageUp
                    | Action::Top
                    | Action::Bottom),
                ) => app.help_nav(action),
                Some(_) => app.close_help(),
                None if input.is_pending() => {}
                None => app.close_help(),
            }
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
        // A version chosen from the compare picker is reconstructed and shown in
        // place by the host, which reads the journalled diffs off disk.
        if let Some(request) = app.take_pending_compare() {
            compare(&mut app, request);
        }
        // A refresh chosen from the launch prompt recaptures the source, just as
        // the refresh action does.
        if app.take_pending_refresh() {
            refresh(&mut app);
        }
        // A quit action or a confirmed picker choice settles how to leave.
        if let Some(exit) = app.pending_exit() {
            return Ok((exit, app.take_drafts()));
        }
        // Fold in another actor's committed changes after handling a key, not
        // only when the loop next goes idle, so a reviewer navigating steadily
        // (keys arriving faster than the poll interval) still sees updates
        // without pausing. The modal state is recomputed here because the key
        // just handled may have opened or closed one; a fold is skipped while a
        // modal owns a spot or buffer it would disturb, and the watcher holds
        // the change until the modal closes. The change gate is a cheap stat, so
        // syncing per key costs almost nothing when the file is untouched.
        if !(app.composing() || app.searching() || app.picking() || app.helping()) {
            sync(&mut app);
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
        install_panic_hook();
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Undo the setup [`TerminalGuard::enter`] performed: leave raw mode and the
/// alternate screen and make the cursor visible again. Best-effort and
/// idempotent, so both the guard's `Drop` and the panic hook can call it
/// without coordinating over which runs first.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

/// Chain a panic hook that restores the terminal before the default hook runs.
/// A panic while the review owns the alternate screen would otherwise print its
/// message and backtrace onto that screen, which the process then tears down as
/// it unwinds, leaving the user at a bare shell with no clue why. Restoring
/// first puts the panic output on the primary screen the user is returned to.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
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
    use crate::render::testutil::{file, theme};
    use crate::review::Review;

    /// A review over a one-line added file authored by a human, for driving the
    /// exit picker after a draft is made.
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
            DiffView::new(theme()).expect("view"),
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
        let document = DiffView::new(theme()).expect("view").render(&diff);
        let app = App::new(document, 0, &theme());

        // A 30x4 screen shows the three rendered rows over the top three lines
        // and the status line filling the last, each padded to 30 columns. The
        // cursor opens centered, halfway through the three-row view.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(30, 4, app),
            "modified  src/lib.rs          \n",
            "@@ -1,1 +1,1 @@               \n",
            "   1    1   let x = 1;        \n",
            "src/lib.rs         0 open  50%\n",
        );
    }

    #[test]
    fn pressing_h_opens_the_help_overlay_centered_over_the_view() {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Context, "let x = 1;", 1)],
            )],
        };
        let document = DiffView::new(theme()).expect("view").render(&diff);
        let mut app = App::new(document, 0, &theme());
        app.update(Action::Help);

        // The overlay floats centered over the diff, clearing the cells behind
        // it: a border titled "Key bindings", the first bindings grouped under
        // their heading with keys and descriptions, then the dismissal hint. The
        // reference is taller than the screen, so a scrollbar runs down the right
        // border with its thumb at the top where the window opens.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(70, 14, app),
            "m┌Key bindings──────────────────────────────────────────────────────┐ \n",
            "@│Navigation                                                        █ \n",
            " │  down, j                  Move down one line                     █ \n",
            " │  up, k                    Move up one line                       ║ \n",
            " │  space, ctrl-f, pagedown  Scroll down one page                   ║ \n",
            " │  b, ctrl-b, pageup        Scroll up one page                     ║ \n",
            " │  g, <, home               Jump to the top                        ║ \n",
            " │  G, >, end                Jump to the bottom                     ║ \n",
            " │  .                        Next file                              ║ \n",
            " │  ,                        Previous file                          ║ \n",
            " │                                                                  │ \n",
            " │any key to close                                                  │ \n",
            " └──────────────────────────────────────────────────────────────────┘ \n",
            "src/lib.rs                                                0 open  100%\n",
        );
    }

    #[test]
    fn a_side_by_side_editor_stands_in_its_target_column() {
        use crate::render::DiffMode;
        // Commenting on the added line, whose after-side lives in the right
        // column, opens the editor scoped to that column: it stands under the
        // right-column content with the left column showing the before-side and
        // the divider running unbroken through the editor band. The anchor-rail
        // corner below sits in the right gutter, where the comment anchors.
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
        let view = DiffView::new(theme()).expect("view");
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let mut app = App::reviewing(Review::new(view, diff, author, 0, Vec::new()), 0, &theme())
            .with_diff_mode(DiffMode::SideBySide, 0);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for c in "why 2?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(60, 9, app),
            "Review [press c here to draft the review comment]           \n",
            "modified  src/lib.rs                                        \n",
            "@@ -1,2 +1,2 @@                                             \n",
            "   1   let x = 1;            │   1   let x = 1;             \n",
            "                             │┌ new comment  src/lib.rs:2  ┐\n",
            "                             ││why 2?                      │\n",
            "                             │└────────────────────────────┘\n",
            "                             │   2 +└let y = 2;             \n",
            "src/lib.rs                                      0 open  100%\n",
        );
    }

    #[test]
    fn editing_a_side_by_side_comment_reopens_the_editor_in_its_column() {
        use crate::render::DiffMode;
        use ulid::Ulid;
        use wiff_core::record::CommentTarget;
        use wiff_core::review::CommentState;
        use wiff_diff::Side;
        // An existing after-side comment on the added line renders its box in the
        // right column. Editing it reopens the editor in that same column,
        // seeded with the body, standing where the box was.
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
        let comment = CommentState {
            id: Ulid(2),
            author: Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            target: CommentTarget::Lines {
                file: "src/lib.rs".to_string(),
                side: Side::After,
                start_line: crate::render::testutil::ln(2),
                end_line: crate::render::testutil::ln(2),
            },
            version: 0,
            anchor: None,
            body: "why 2?".to_string(),
            resolved: false,
            resolved_by: None,
            deleted: false,
            deleted_by: None,
            confidence: None,
            created_seq: 0,
            updated_seq: 0,
        };
        let view = DiffView::new(theme()).expect("view");
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let mut app = App::reviewing(
            Review::new(view, diff, author, 0, vec![comment]),
            0,
            &theme(),
        )
        .with_diff_mode(DiffMode::SideBySide, 0);
        app.update(Action::NextComment);
        app.update(Action::EditComment);
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(60, 9, app),
            "Review [press c here to draft the review comment]           \n",
            "modified  src/lib.rs                                        \n",
            "@@ -1,2 +1,2 @@                                             \n",
            "   1   let x = 1;            │   1   let x = 1;             \n",
            "                             │┌ edit comment  ctrl-d submit┐\n",
            "                             ││why 2?                      │\n",
            "                             │└────────────────────────────┘\n",
            "                             │   2 +└let y = 2;             \n",
            "src/lib.rs                                       1 open  57%\n",
        );
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
        let view = DiffView::new(theme()).expect("view");
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let mut app = App::reviewing(Review::new(view, diff, author, 0, Vec::new()), 0, &theme());
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
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(30, 9, app),
            "Review [press c here to draft \n",
            "modified  src/lib.rs          \n",
            "@@ -1,2 +1,2 @@               \n",
            "   1    1   let x = 1;        \n",
            "┌ new comment  src/lib.rs:2  ┐\n",
            "│why 2?                      │\n",
            "└────────────────────────────┘\n",
            "        2 +└let y = 2;        \n",
            "src/lib.rs        0 open  100%\n",
        );
    }

    /// The screen after drawing `app`, each row shown as its symbols and then a
    /// mask marking cells the cursor tint covers with `#` and the rest with `.`,
    /// so the cursor line's tint around the floating editor is asserted. Drawing
    /// through the shared loop settles the width, height, and initial cursor
    /// position, so callers draw once to settle before interacting.
    fn screen_tinted(width: u16, height: u16, app: &mut App) -> String {
        let tint = crate::render::color(theme().cursor_bg);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        draw(&mut terminal, app).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for row in 0..height {
            for col in 0..width {
                out.push_str(buffer[(col, row)].symbol());
            }
            out.push_str("  ");
            for col in 0..width {
                out.push(if buffer[(col, row)].bg == tint {
                    '#'
                } else {
                    '.'
                });
            }
            out.push('\n');
        }
        out
    }

    /// A review over a six-line added file, for the detached-editor tint tests.
    fn six_line_review() -> Review {
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[
                    (LineKind::Added, "let v1 = 1;", 1),
                    (LineKind::Added, "let v2 = 2;", 2),
                    (LineKind::Added, "let v3 = 3;", 3),
                    (LineKind::Added, "let v4 = 4;", 4),
                    (LineKind::Added, "let v5 = 5;", 5),
                    (LineKind::Added, "let v6 = 6;", 6),
                ],
            )],
        };
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        Review::new(
            DiffView::new(theme()).expect("view"),
            diff,
            author,
            0,
            Vec::new(),
        )
    }

    #[test]
    fn the_detached_editor_tints_the_whole_rule_when_the_cursor_is_on_the_anchor() {
        let mut app = App::reviewing(six_line_review(), 12, &theme());
        // Settle the width, height, and initial cursor position before moving.
        screen_tinted(30, 12, &mut app);
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for c in "why 2?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        // Detaching leaves the cursor on the anchor line, which the box's top
        // border sits over, so the whole top rule shows the cursor tint.
        app.compose_key(KeyPress::with_modifiers(Key::Char('o'), true, false, false));

        #[rustfmt::skip]
        wince::snapshot_str!(
            screen_tinted(30, 12, &mut app),
            "Review [press c here to draft   ..............................\n",
            "modified  src/lib.rs            ..............................\n",
            "@@ -1,6 +1,6 @@                 ..............................\n",
            "        1 + let v1 = 1;         ..............................\n",
            "  ┌ new comment  src/lib.rs┐    ##############################\n",
            "  │why 2?                  │    ..............................\n",
            "  └────────────────────────┘    ..............................\n",
            "        5 + let v5 = 5;         ..............................\n",
            "        6 + let v6 = 6;         ..............................\n",
            "                                ..............................\n",
            "                                ..............................\n",
            "src/lib.rs         0 open  50%  ..............................\n",
        );
    }

    #[test]
    fn the_detached_editor_carries_the_cursor_tint_around_its_chrome() {
        let mut app = App::reviewing(six_line_review(), 12, &theme());
        // Settle the width, height, and initial cursor position before moving.
        screen_tinted(30, 12, &mut app);
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for c in "why 2?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        // Detach with ctrl-o, then step the cursor down one so it sits on the
        // line the box body covers: the cursor tint reads on the box's left and
        // right border cells at that row rather than as a full-width line.
        app.compose_key(KeyPress::with_modifiers(Key::Char('o'), true, false, false));
        app.compose_key(KeyPress::new(Key::Char('j')));

        #[rustfmt::skip]
        wince::snapshot_str!(
            screen_tinted(30, 12, &mut app),
            "Review [press c here to draft   ..............................\n",
            "modified  src/lib.rs            ..............................\n",
            "@@ -1,6 +1,6 @@                 ..............................\n",
            "        1 + let v1 = 1;         ..............................\n",
            "  ┌ new comment  src/lib.rs┐    ..............................\n",
            "  │why 2?                  │    ###........................###\n",
            "  └────────────────────────┘    ..............................\n",
            "        5 + let v5 = 5;         ..............................\n",
            "        6 + let v6 = 6;         ..............................\n",
            "                                ..............................\n",
            "                                ..............................\n",
            "src/lib.rs         0 open  62%  ..............................\n",
        );
    }

    #[test]
    fn the_detached_editor_names_the_detach_key_in_its_title() {
        let mut app = App::reviewing(six_line_review(), 10, &theme());
        // Settle the width, height, and initial cursor position before moving.
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("test terminal");
        draw(&mut terminal, &mut app).expect("draw");
        app.update(Action::Top);
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for c in "why 2?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        // ctrl-o floats the editor and hands the cursor to the diff, so the
        // title names the detach key reading `edit`, sitting before the cancel
        // key.
        app.compose_key(KeyPress::with_modifiers(Key::Char('o'), true, false, false));

        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(80, 10, app),
            "Review [press c here to draft the review comment]                               \n",
            "modified  src/lib.rs                                                            \n",
            "@@ -1,6 +1,6 @@                                                                 \n",
            "        1 + let v1 = 1;                                                         \n",
            "  ┌ new comment  src/lib.rs:2  ctrl-d submit  [ctrl-o edit]  esc cancel ─────┐  \n",
            "  │why 2?                                                                    │  \n",
            "  └──────────────────────────────────────────────────────────────────────────┘  \n",
            "        5 + let v5 = 5;                                                         \n",
            "        6 + let v6 = 6;                                                         \n",
            "src/lib.rs                                                           0 open  50%\n",
        );
    }

    #[test]
    fn draws_a_scrollbar_when_the_editor_body_overflows_the_box() {
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
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let mut app = App::reviewing(
            Review::new(
                DiffView::new(theme()).expect("view"),
                diff,
                author,
                0,
                Vec::new(),
            ),
            0,
            &theme(),
        );
        // Open the editor on the added line and type more lines than the capped
        // box can show.
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for i in 1..=8 {
            for c in format!("line {i}").chars() {
                app.compose_key(KeyPress::new(Key::Char(c)));
            }
            if i < 8 {
                app.compose_key(KeyPress::new(Key::Enter));
            }
        }

        // A 22-row screen leaves a 20-row document area, capping the editor
        // interior at 4 rows, so the eight-line body overflows: the last four
        // lines show, a scrollbar runs down the right border with its thumb near
        // the bottom where the cursor rests, and the leftover rows fall to the
        // blank document area below the box.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(30, 22, app),
            "Review [press c here to draft \n",
            "modified  src/lib.rs          \n",
            "@@ -1,2 +1,2 @@               \n",
            "   1    1   let x = 1;        \n",
            "┌ new comment  src/lib.rs:2  ┐\n",
            "│line 5                      ║\n",
            "│line 6                      ║\n",
            "│line 7                      █\n",
            "│line 8                      █\n",
            "└────────────────────────────┘\n",
            "        2 +└let y = 2;        \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "src/lib.rs        0 open  100%\n",
        );
    }

    #[test]
    fn a_body_over_the_box_by_one_row_draws_a_nearly_full_thumb() {
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
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let mut app = App::reviewing(
            Review::new(
                DiffView::new(theme()).expect("view"),
                diff,
                author,
                0,
                Vec::new(),
            ),
            0,
            &theme(),
        );
        for _ in 0..4 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for i in 1..=5 {
            for c in format!("line {i}").chars() {
                app.compose_key(KeyPress::new(Key::Char(c)));
            }
            if i < 5 {
                app.compose_key(KeyPress::new(Key::Enter));
            }
        }

        // The box shows 4 of the body's 5 rows, off by a single row, so the
        // thumb nearly fills the track (3 of its 4 cells) rather than sitting at
        // half height.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(30, 22, app),
            "Review [press c here to draft \n",
            "modified  src/lib.rs          \n",
            "@@ -1,2 +1,2 @@               \n",
            "   1    1   let x = 1;        \n",
            "┌ new comment  src/lib.rs:2  ┐\n",
            "│line 2                      ║\n",
            "│line 3                      █\n",
            "│line 4                      █\n",
            "│line 5                      █\n",
            "└────────────────────────────┘\n",
            "        2 +└let y = 2;        \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "                              \n",
            "src/lib.rs        0 open  100%\n",
        );
    }

    #[test]
    fn draws_the_exit_picker_centered_over_the_view_with_pending_drafts() {
        let mut app =
            App::reviewing(draft_review(), 0, &theme()).with_exit_default(ExitDefault::Prompt);
        // Author a comment so a draft is pending, then quit to raise the picker.
        app.update(Action::AddComment);
        for c in "why?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        app.compose_key(KeyPress::with_modifiers(Key::Char('d'), true, false, false));
        app.update(Action::Quit);

        // The three-choice picker floats centered over the review, the first
        // choice highlighted with its marker and the key hint along the bottom.
        // It clears the cells behind it, so the underlying diff shows only where
        // the box does not cover it.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(50, 11, app),
            "Review [press c here to draft the review comment] \n",
            "┌ we┌You have uncommitted comments───────────┐esol\n",
            "│why│> Commit review                         │   │\n",
            "└───│  Quit without saving                   │───┘\n",
            "modi│  Remove session                        │    \n",
            "@@ -│                                        │    \n",
            "    │  up/down move  enter select  esc cancel│    \n",
            "    └────────────────────────────────────────┘    \n",
            "                                                  \n",
            "                                                  \n",
            "src/lib.rs                           * 1 open  83%\n",
        );
    }
}
