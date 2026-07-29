//! The terminal event loop that drives a review.
//!
//! [`run`] takes over the terminal, draws the [`App`], and turns each key event
//! into an action through the [`Keymap`]: navigation moves the view, and a quit
//! action ends the loop and reports how the reviewer chose to leave. It is the
//! imperative shell around the pure [`App`], [`Input`], and rendering layers;
//! the terminal is always restored, including on a draw or read error.

use std::io::{self, Stdout};
use std::time::Duration;

use nix::sys::signal::{Signal, raise};
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
use crate::app::{App, CompareRequest, ComposeColumn, ComposeView, FloatView, PublishStep, Update};
use crate::event::to_key_press;
use crate::exit::Exit;
use crate::input::Input;
use crate::key::Key;
use crate::keymap::Keymap;
use crate::notice::Notice;
use crate::picker::Picker;
use crate::render::{COLUMN_DIVIDER, color};

/// How long the loop waits for a key before waking to pick up another actor's
/// changes to the session. Long enough that an idle review costs almost nothing,
/// short enough that an update arrives promptly.
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// The poll interval used while background highlighting is in progress, shorter
/// than [`POLL_INTERVAL`] so arriving parses reveal without waiting a full poll
/// cycle.
const HIGHLIGHT_POLL_INTERVAL: Duration = Duration::from_millis(30);

/// A host callback that updates the review in place.
type Hook<'a> = Box<dyn FnMut(&mut App) + 'a>;

/// A host callback that updates the review in place and reports whether it
/// changed anything.
type ReportingHook<'a> = Box<dyn FnMut(&mut App) -> bool + 'a>;

/// A host callback that updates the review in place given a chosen value.
type ChoiceHook<'a, T> = Box<dyn FnMut(&mut App, T) + 'a>;

/// The host operations the review loop calls back into when the reviewer asks
/// for something that reads or writes the session on disk, which the app never
/// touches itself. Each is called with the app to update in place; a failure is
/// the host's to report through the app's status line or a modal notice.
pub struct Hooks<'a> {
    /// Recapture the source as a new version and reload the review over it.
    pub refresh: Hook<'a>,
    /// Commit the pending draft comments, keeping the review open.
    pub save: Hook<'a>,
    /// Fold in another actor's committed changes, returning whether anything
    /// changed so the loop repaints only when it did.
    pub sync: ReportingHook<'a>,
    /// Reconstruct a chosen version comparison and show it in place.
    pub compare: ChoiceHook<'a, CompareRequest>,
    /// Perform a publish phase: open the prompt, reconcile the forge, or send.
    pub publish: ChoiceHook<'a, PublishStep>,
    /// Walk the repository for files an explore review can add and open the
    /// picker over them.
    pub list_files: Hook<'a>,
    /// Widen an explore review with the chosen path, reload over the new
    /// version, and move the cursor to the added file.
    pub add_file: ChoiceHook<'a, String>,
}

/// Run the review loop over `app`, resolving key events through `keymap`, until
/// a quit action ends it. Everything that touches the session on disk or the
/// forge is handed to the matching hook in `hooks`: a [`Action::Refresh`]
/// recaptures and reloads the app in place, a [`Action::Save`] commits the
/// pending drafts and keeps the review open, choosing a version to compare
/// against reconstructs that diff, an [`Action::Publish`] and each confirmed
/// publish phase drive the pull-and-push round-trip to the forge, and
/// [`Action::AddFile`] walks the repository and, on a choice, widens an explore
/// review; each reports its own outcome through the app's status line. After
/// each key and whenever the review sits idle the loop calls the sync hook,
/// which picks up another actor's updates to the session and folds them into the
/// app, returning whether it changed anything so the loop repaints only when it
/// did. The terminal is put into raw mode on an alternate screen for the
/// duration and restored before returning. Returns how the reviewer chose to
/// leave together with any buffered draft edits still to commit.
pub fn run(app: App, keymap: Keymap, hooks: Hooks) -> io::Result<(Exit, Vec<RecordBody>)> {
    let mut terminal = TerminalGuard::enter()?;
    event_loop(&mut terminal.terminal, app, keymap, hooks)
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
        if app.noticing() {
            render_notice(frame, doc_area, app);
        }
        // The busy modal paints last, over any prompt it replaced, while a
        // blocking host operation holds the loop.
        if app.working().is_some() {
            render_working(frame, doc_area, app);
        }
    })?;
    Ok(())
}

/// The centered rectangle, content width, and visible line count for a scrolling
/// modal overlay of `total` content lines and `content_width` columns floating
/// over `area`. The border, spacer, and hint take four rows, plus `pinned_top`
/// rows for anything pinned above the scrolling content (a filter prompt); the
/// rest is the room the content has, so an overlay taller than that scrolls
/// rather than overflowing.
fn centered_modal(
    area: Rect,
    total: usize,
    content_width: usize,
    pinned_top: usize,
) -> (Rect, usize, usize) {
    let chrome = 4u16 + pinned_top as u16;
    let visible = total.min(area.height.saturating_sub(chrome) as usize);
    let inner = modal_inner_width(area, content_width);
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

/// The usable inner width of a modal of `content_width` columns over `area`,
/// accounting for its border. Shared so callers measuring content ahead of
/// `centered_modal` wrap to the same width it lays out at.
fn modal_inner_width(area: Rect, content_width: usize) -> usize {
    content_width.min(area.width.saturating_sub(2) as usize)
}

/// Draw the modal list centered over `area`, clearing the cells behind it, its
/// window sized so a list taller than the space scrolls rather than overflowing.
fn render_picker(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some((rows, content_width)) = app.picker().map(|p| (p.list_len(), p.width())) else {
        return;
    };
    // The note wraps to the width the modal lays out at and scrolls with the
    // rows, so it counts toward the scrollable content measured at that width.
    let lead = app
        .picker()
        .map_or(0, |p| p.note_height(modal_inner_width(area, content_width)));
    let pinned_top = app.picker().map_or(0, Picker::prompt_height);
    let total = rows + lead;
    let (rect, inner, visible) = centered_modal(area, total, content_width, pinned_top);
    app.picker_set_viewport(visible, lead);
    let Some(picker) = app.picker() else {
        return;
    };
    render_scrolling_modal(
        frame,
        rect,
        ScrollingModal {
            title: picker.title(),
            border: picker.border(),
            background: picker.background(),
            lines: picker.lines(inner),
            top: picker.top(),
            total,
            visible,
            pinned_top,
        },
    );
}

/// A framed, scrollable modal ready to draw over the view: its titled border
/// colors, the content lines filling it, and where the scrollable window sits.
struct ScrollingModal<'a> {
    title: &'a str,
    border: Rgb,
    background: Rgb,
    lines: Vec<ratatui::text::Line<'static>>,
    /// The first visible content line, for the scrollbar thumb.
    top: usize,
    /// The total content lines, and how many the window shows; a scrollbar is
    /// drawn only when the former exceeds the latter.
    total: usize,
    visible: usize,
    /// Lines pinned above the scrolling window (a filter prompt), which the
    /// scrollbar track starts below.
    pinned_top: usize,
}

/// Draw `modal` into `rect`, clearing the cells behind it, framing it with a
/// titled border, and running a scrollbar down the right border when its content
/// is taller than the window. Shared by the picker, the help overlay, and the
/// error notice, which frame and scroll alike.
fn render_scrolling_modal(frame: &mut Frame, rect: Rect, modal: ScrollingModal) {
    let background = Style::default().bg(color(modal.background));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(background.fg(color(modal.border)))
        .style(background)
        .title(modal.title.to_string());
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(modal.lines).block(block), rect);
    // More content than the window shows draws a scrollbar down the right border,
    // spanning the content rows above the spacer and hint, so the reviewer can
    // see there is more off-screen.
    if modal.total > modal.visible {
        let mut state = ScrollbarState::new(modal.total.saturating_sub(modal.visible) + 1)
            .position(modal.top)
            .viewport_content_length(modal.visible);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        let track = Rect {
            y: rect.y + 1 + modal.pinned_top as u16,
            height: modal.visible as u16,
            ..rect
        };
        frame.render_stateful_widget(scrollbar, track, &mut state);
    }
}

/// Draw the help overlay centered over `area`, clearing the cells behind it, its
/// window sized so a reference taller than the space scrolls rather than
/// overflowing.
fn render_help(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some((total, content_width)) = app.help().map(|h| (h.list_len(), h.width())) else {
        return;
    };
    let (rect, inner, visible) = centered_modal(area, total, content_width, 0);
    app.help_set_height(visible);
    let Some(help) = app.help() else {
        return;
    };
    render_scrolling_modal(
        frame,
        rect,
        ScrollingModal {
            title: help.title(),
            border: help.border(),
            background: help.background(),
            lines: help.lines(inner),
            top: help.top(),
            total,
            visible,
            pinned_top: 0,
        },
    );
}

/// Draw the error notice centered over `area`, clearing the cells behind it,
/// its window sized so a message taller than the space scrolls rather than
/// overflowing.
fn render_notice(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(content_width) = app.notice().map(|n| n.width()) else {
        return;
    };
    let inner = modal_inner_width(area, content_width);
    let total = app.notice_wrap(inner);
    let (rect, _, visible) = centered_modal(area, total, content_width, 0);
    app.notice_set_height(visible);
    let Some(notice) = app.notice() else {
        return;
    };
    render_scrolling_modal(
        frame,
        rect,
        ScrollingModal {
            title: notice.title(),
            border: notice.border(),
            background: notice.background(),
            lines: notice.lines(),
            top: notice.top(),
            total,
            visible,
            pinned_top: 0,
        },
    );
}

/// Draw the busy modal centered over `area`, naming the blocking host operation
/// that holds the loop while it runs.
fn render_working(frame: &mut Frame, area: Rect, app: &App) {
    let Some((title, body)) = app.working() else {
        return;
    };
    let mut notice = Notice::working(title.to_string(), body.to_string(), app.working_colors());
    let content_width = notice.width();
    let inner = modal_inner_width(area, content_width);
    let total = notice.wrap_to(inner);
    let (rect, _, visible) = centered_modal(area, total, content_width, 0);
    notice.set_height(visible);
    render_scrolling_modal(
        frame,
        rect,
        ScrollingModal {
            title: notice.title(),
            border: notice.border(),
            background: notice.background(),
            lines: notice.lines(),
            top: notice.top(),
            total,
            visible,
            pinned_top: 0,
        },
    );
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
    mut hooks: Hooks,
) -> io::Result<(Exit, Vec<RecordBody>)> {
    let mut input = Input::new(keymap);
    // Repaint only when the view might have changed, so an idle poll that finds
    // no session update does not redraw. Set the first time through to paint the
    // opening frame.
    let mut dirty = true;
    loop {
        // A modal view -- the comment editor, search prompt, or picker (the
        // file, comment, theme, or exit list) -- owns a spot or buffer that
        // folding another actor's committed change in would disturb.
        let modal =
            app.composing() || app.searching() || app.picking() || app.helping() || app.noticing();
        // Fold in arrived highlights even under a modal. A highlight recolors
        // the document without changing its structure; it cannot disturb a spot
        // a modal holds. Repaint immediately only when the change is on screen.
        if app.poll_highlights() {
            dirty = true;
        }
        if dirty {
            draw(terminal, &mut app)?;
            dirty = false;
        }
        // A wait bounded by the poll interval between key presses, or
        // HIGHLIGHT_POLL_INTERVAL while background highlighting runs.
        let timeout = if app.highlighting() {
            HIGHLIGHT_POLL_INTERVAL
        } else {
            POLL_INTERVAL
        };
        if !event::poll(timeout)? {
            if !modal && (hooks.sync)(&mut app) {
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
        } else if app.noticing() {
            // The notice is a message to read: navigation scrolls it and every
            // other key closes it, the same way the help overlay behaves.
            match input.press(press) {
                Some(
                    action @ (Action::LineDown
                    | Action::LineUp
                    | Action::PageDown
                    | Action::PageUp
                    | Action::Top
                    | Action::Bottom),
                ) => app.notice_nav(action),
                Some(_) => app.close_notice(),
                None if input.is_pending() => {}
                None => app.close_notice(),
            }
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
            // Enter activates the highlight. The list starts in navigation mode,
            // where the keymap moves the highlight with the reviewer's own
            // bindings and escape closes the list. In a filterable list `/`
            // enters filter mode; there a printable press edits the query,
            // backspace trims it, and escape returns to navigation, while the
            // arrows still move the highlight through the narrowed rows.
            match press.key {
                Key::Enter => app.picker_activate(),
                Key::Escape if app.picker_filtering() => app.picker_end_filter(),
                Key::Escape => app.picker_cancel(),
                Key::Backspace if app.picker_filtering() => app.picker_pop_query(),
                Key::Char('/')
                    if app.picker_filterable()
                        && !app.picker_filtering()
                        && !press.ctrl
                        && !press.alt =>
                {
                    app.picker_begin_filter()
                }
                Key::Char(ch) if app.picker_filtering() && !press.ctrl && !press.alt => {
                    app.picker_push_query(ch)
                }
                _ => {
                    if let Some(action) = input.press(press) {
                        app.picker_nav(action);
                    }
                }
            }
        } else if let Some(action) = input.press(press) {
            // Refresh and save are handed to the host: refresh recaptures and
            // reloads the app in place, save commits the pending drafts. Suspend
            // the loop handles itself, stopping the process and repainting on
            // resume.
            match app.update(action) {
                Update::Passed(Action::Refresh) => {
                    // A forge fetch blocks the loop on the network, so paint its
                    // busy modal before the round-trip; a local refresh has none
                    // and paints nothing.
                    if app.begin_refresh_working() {
                        draw(terminal, &mut app)?;
                    }
                    (hooks.refresh)(&mut app);
                    app.end_working();
                }
                Update::Passed(Action::Save) => (hooks.save)(&mut app),
                Update::Passed(Action::Publish) => {
                    (hooks.publish)(&mut app, PublishStep::Requested)
                }
                Update::Passed(Action::Suspend) => {
                    suspend()?;
                    // The alternate screen comes back blank on resume, so
                    // discard the prior frame and repaint the whole view.
                    terminal.clear()?;
                }
                _ => {}
            }
        }
        // A version chosen from the compare picker is reconstructed and shown in
        // place by the host, which reads the journalled diffs off disk.
        if let Some(request) = app.take_pending_compare() {
            (hooks.compare)(&mut app, request);
        }
        if app.take_pending_file_list() {
            (hooks.list_files)(&mut app);
        }
        if let Some(path) = app.take_pending_add_file() {
            (hooks.add_file)(&mut app, path);
        }
        // A refresh chosen from the launch prompt recaptures the source, just as
        // the refresh action does, painting the forge busy modal first when the
        // review has one.
        if app.take_pending_refresh() {
            if app.begin_refresh_working() {
                draw(terminal, &mut app)?;
            }
            (hooks.refresh)(&mut app);
            app.end_working();
        }
        // A publish phase confirmed from a publish prompt is performed by the
        // host, which reconciles the forge and then sends the review back. The
        // forge round-trip blocks the loop, so paint a modal naming it first,
        // giving the reviewer a sign of progress rather than a frozen screen.
        if let Some(step) = app.take_pending_publish() {
            app.begin_publishing(step);
            draw(terminal, &mut app)?;
            (hooks.publish)(&mut app, step);
            app.end_working();
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
        if !(app.composing() || app.searching() || app.picking() || app.helping() || app.noticing())
        {
            (hooks.sync)(&mut app);
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
        take_over_terminal()?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        install_panic_hook();
        Ok(Self { terminal })
    }
}

/// Put the terminal into raw mode on a fresh alternate screen. Both the first
/// takeover and resuming from a suspend use this; the panic hook is installed
/// once by [`TerminalGuard::enter`], not here.
fn take_over_terminal() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    Ok(())
}

/// Suspend wiff into the shell's job control. Blocks until the process is
/// continued, then retakes the alternate screen in raw mode.
fn suspend() -> io::Result<()> {
    restore_terminal();
    // A stop signal stops every thread, the background highlight pool included,
    // and hands the terminal back to the shell. raise returns once the process
    // is continued with SIGCONT; in an orphaned process group the kernel
    // discards the stop and it returns without ever stopping.
    raise(Signal::SIGTSTP).map_err(io::Error::other)?;
    take_over_terminal()
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

    use time::OffsetDateTime;
    use wiff_core::record::{Author, AuthorKind, Seq, VersionNumber};

    use super::draw;
    use crate::action::Action;
    use crate::app::{App, PublishStep};
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
            None,
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
            " │  down, j                  Move down one line                     ║ \n",
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
    fn a_notice_floats_wrapped_and_centered_over_the_view() {
        // A message too long for the one-row status line is raised as a notice
        // whose text wraps across a centered modal, titled and cleared over the
        // diff behind it, with the dismissal hint below.
        let diff = Diff {
            files: vec![file(
                "src/lib.rs",
                FileStatus::Modified,
                &[(LineKind::Context, "let x = 1;", 1)],
            )],
        };
        let document = DiffView::new(theme()).expect("view").render(&diff);
        let mut app = App::new(document, 0, &theme());
        // A clearly synthetic body, so the fixture does not impersonate a real
        // message and cannot fall out of step with one as the wording changes.
        app.show_notice(
            "Notice",
            "The quick brown fox jumps over the lazy dog and then keeps running \
             well past the edge of the modal so the text has to wrap.",
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(72, 12, app),
            "modified  src/lib.rs                                                    \n",
            "@@ -1,1 +1,1 @@                                                         \n",
            "   1 ┌Notice──────────────────────────────────────────────────────┐     \n",
            "     │The quick brown fox jumps over the lazy dog and then keeps  │     \n",
            "     │running well past the edge of the modal so the text has to  │     \n",
            "     │wrap.                                                       │     \n",
            "     │                                                            │     \n",
            "     │any key to close                                            │     \n",
            "     └────────────────────────────────────────────────────────────┘     \n",
            "                                                                        \n",
            "                                                                        \n",
            "src/lib.rs                                                  0 open  100%\n",
        );
    }

    #[test]
    fn the_publishing_modal_names_the_pull_request_centered_over_the_view() {
        // While a publish blocks the loop, a modal floats centered over the
        // diff, titled "Publishing", naming the bound pull request and hinting
        // to wait rather than to dismiss.
        let mut app = App::reviewing(draft_review(), 0, &theme());
        app.offer_publish("https://github.com/octo/demo/pull/7");
        app.picker_cancel();
        app.begin_publishing(PublishStep::Publish);
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(72, 10, app),
            "Review [press c here to draft the review comment] [press e to write the \n",
            "modified  src/lib.rs                                                    \n",
            "@@ -1,┌Publishing────────────────────────────────────────────────┐      \n",
            "      │Sending your review to https://github.com/octo/demo/pull/7│      \n",
            "      │                                                          │      \n",
            "      │please wait                                               │      \n",
            "      └──────────────────────────────────────────────────────────┘      \n",
            "                                                                        \n",
            "                                                                        \n",
            "src/lib.rs                                                  0 open  100%\n",
        );
    }

    #[test]
    fn the_forge_fetch_modal_floats_centered_over_the_view_while_it_blocks() {
        // While a forge fetch blocks the loop, its busy modal floats centered
        // over the diff, titled "Fetching", naming the source and hinting to
        // wait rather than to dismiss.
        let mut app = App::reviewing(draft_review(), 0, &theme())
            .with_refresh_modal("Fetching", "Fetching the latest from octo/demo#7");
        app.begin_refresh_working();
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(72, 10, app),
            "Review [press c here to draft the review comment] [press e to write the \n",
            "modified  src/lib.rs                                                    \n",
            "@@ -1,1 +1,1 @@  ┌Fetching────────────────────────────┐                 \n",
            "        1 + let y│Fetching the latest from octo/demo#7│                 \n",
            "                 │                                    │                 \n",
            "                 │please wait                         │                 \n",
            "                 └────────────────────────────────────┘                 \n",
            "                                                                        \n",
            "                                                                        \n",
            "src/lib.rs                                                  0 open  100%\n",
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
        let mut app = App::reviewing(
            Review::new(view, diff, author, 0, Vec::new(), None),
            0,
            &theme(),
        )
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
            "Review [press c here to draft the review comment] [press e t\n",
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
            version: VersionNumber(0),
            anchor: None,
            body: "why 2?".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
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
            number: Some(wiff_core::record::CommentNumber(1)),
            created_seq: Seq(0),
            updated_seq: Seq(0),
        };
        let view = DiffView::new(theme()).expect("view");
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let mut app = App::reviewing(
            Review::new(view, diff, author, 0, vec![comment], None),
            0,
            &theme(),
        )
        .with_diff_mode(DiffMode::SideBySide, 0);
        app.update(Action::NextComment);
        app.update(Action::EditComment);
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(60, 9, app),
            "Review [press c here to draft the review comment] [press e t\n",
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
    fn replying_to_a_side_by_side_comment_opens_the_editor_in_its_column() {
        use crate::render::DiffMode;
        use ulid::Ulid;
        use wiff_core::record::CommentTarget;
        use wiff_core::review::CommentState;
        use wiff_diff::Side;
        // An after-side comment on the added line renders its box in the right
        // column. Replying threads a new editor beneath it in that same column,
        // not spanning the full width.
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
            version: VersionNumber(0),
            anchor: None,
            body: "why 2?".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
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
            number: Some(wiff_core::record::CommentNumber(1)),
            created_seq: Seq(0),
            updated_seq: Seq(0),
        };
        let view = DiffView::new(theme()).expect("view");
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let mut app = App::reviewing(
            Review::new(view, diff, author, 0, vec![comment], None),
            0,
            &theme(),
        )
        .with_diff_mode(DiffMode::SideBySide, 0);
        app.update(Action::NextComment);
        app.update(Action::ReplyComment);
        for c in "agreed".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(60, 9, app),
            "@@ -1,2 +1,2 @@                                             \n",
            "   1   let x = 1;            │   1   let x = 1;             \n",
            "                             │┌ #1 wez (human)  press e to ┐\n",
            "                             ││why 2?                      │\n",
            "                             │└─────┬──────────────────────┘\n",
            "                             │┌ reply  ctrl-d submit  esc c┐\n",
            "                             ││agreed                      │\n",
            "                             │└────────────────────────────┘\n",
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
        let mut app = App::reviewing(
            Review::new(view, diff, author, 0, Vec::new(), None),
            0,
            &theme(),
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
            None,
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
            "Review [press c here to draft the review comment] [press e to write the descript\n",
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
                None,
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
                None,
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
            "┌ we┌You have uncommitted comments───────────┐eply\n",
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

    #[test]
    fn a_picker_taller_than_its_window_runs_a_scrollbar_down_its_border() {
        let diff = Diff {
            files: (0..10)
                .map(|n| {
                    file(
                        &format!("src/file{n:02}.rs"),
                        FileStatus::Modified,
                        &[(LineKind::Context, "let x = 1;", 1)],
                    )
                })
                .collect(),
        };
        let document = DiffView::new(theme()).expect("view").render(&diff);
        let mut app = App::new(document, 0, &theme());
        app.update(Action::PickFile);

        // The ten-file list is taller than the four rows the window leaves after
        // the border, spacer, and hint, so a scrollbar runs down the right
        // border, its thumb at the top over the first files.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(40, 9, app),
            "┌Jump to file──────────────────────────┐\n",
            "│> src/file00.rs                       █\n",
            "│  src/file01.rs                       █\n",
            "│  src/file02.rs                       ║\n",
            "│  src/file03.rs                       ║\n",
            "│                                      │\n",
            "│  up/down move  enter select  esc canc│\n",
            "└──────────────────────────────────────┘\n",
            "src/file01.rs                0 open  13%\n",
        );
    }
}
