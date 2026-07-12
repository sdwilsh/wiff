//! The inline editor for authoring or revising a comment body.
//!
//! A [`Compose`] wraps a [`TextArea`] shown in place of the comment it will
//! become: authoring one anchors it above the line it targets, revising one
//! anchors it above the comment's header. The editor owns its own keys (cursor
//! motion, word and line kills, undo); the app hands it every press except the
//! two that leave it, submit and cancel. Cancelling after the body has changed
//! asks for confirmation first so an accidental keystroke cannot discard work.
//!
//! The editor renders itself rather than leaning on tui-textarea's own widget:
//! its logical lines soft-wrap to the box interior (see [`editor_wrap`]) and its
//! cursor position is reported for the caller to place the terminal's hardware
//! cursor, so a long body wraps instead of scrolling sideways, vertical motion
//! follows the wrapped shape, and an input method's candidate window tracks the
//! real edit point.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use tui_textarea::{CursorMove, Input, Key as EditorKey, TextArea};
use ulid::Ulid;
use wiff_core::record::CommentTarget;
use wiff_diff::{LiveHighlighter, Rgb};

use crate::editor_wrap::{CursorBias, VisualRow, WrapMap};
use crate::key::{Key, KeyPress};
use crate::render::color;

/// What the caller needs to draw one frame of the editor box: the rows to paint
/// inside it, where to put the terminal cursor, and the scroll state.
pub struct EditorView {
    /// The rows drawn inside the editor box.
    pub rows: Vec<Line<'static>>,
    /// The cursor's (column, row) within `rows`, or `None` when it has scrolled
    /// out of view.
    pub cursor: Option<(u16, u16)>,
    /// Present only when the body is taller than the box.
    pub scroll: Option<Scroll>,
}

/// How far a body taller than the editor box is scrolled.
pub struct Scroll {
    /// Index of the first visible row.
    pub offset: usize,
    /// Total wrapped row count.
    pub total: usize,
}

/// What a compose session produces when it is saved.
pub enum ComposeKind {
    /// A new comment on the given target.
    Add(CommentTarget),
    /// A revision to an existing comment's body.
    Edit(Ulid),
}

/// The inline comment editor: a text buffer plus what it will produce, where it
/// renders, and whether a cancel is awaiting confirmation.
pub struct Compose {
    textarea: TextArea<'static>,
    kind: ComposeKind,
    /// The view row the editor renders above.
    anchor: usize,
    /// The body the editor opened with, to detect unsaved changes on cancel.
    original: String,
    /// The heading naming what is being written, shown on the editor border.
    label: String,
    /// The submit/cancel key hint shown alongside the label on the border.
    hint: String,
    /// The color of the editor's border.
    border: Rgb,
    /// Whether a cancel with unsaved changes is awaiting a yes/no answer.
    confirming: bool,
    /// How a cursor on a soft-wrap boundary resolves to a visual row.
    bias: CursorBias,
    /// The column a vertical move aims to keep, remembered across successive
    /// up/down presses; cleared by any edit or horizontal move.
    goal_col: Option<usize>,
    /// Incremental markdown highlighter for the body.
    highlighter: LiveHighlighter,
}

impl Compose {
    /// An editor for `kind`, rendered above view row `anchor`, seeded with
    /// `seed` (empty when authoring), titled `label` with the submit/cancel
    /// `hint` beside it, and bordered in `border`.
    pub fn new(
        kind: ComposeKind,
        anchor: usize,
        seed: &str,
        label: String,
        hint: String,
        border: Rgb,
        mut highlighter: LiveHighlighter,
    ) -> Self {
        let lines: Vec<String> = if seed.is_empty() {
            vec![String::new()]
        } else {
            seed.split('\n').map(str::to_string).collect()
        };
        highlighter.update(&lines);
        let mut textarea = TextArea::new(lines);
        // The editor otherwise underlines the whole line the cursor is on, which
        // reads as emphasis on the text rather than a cursor.
        textarea.set_cursor_line_style(Style::default());
        // Open with the cursor after the seeded text so an edit appends.
        textarea.move_cursor(CursorMove::Bottom);
        textarea.move_cursor(CursorMove::End);
        Self {
            textarea,
            kind,
            anchor,
            original: seed.to_string(),
            label,
            hint,
            border,
            confirming: false,
            bias: CursorBias::Forward,
            goal_col: None,
            highlighter,
        }
    }

    /// Feed a key press to the editor, wrapping to `width` columns. Vertical
    /// motion and the line-bound keys follow the wrapped shape; every other
    /// press edits the text buffer directly.
    pub fn input(&mut self, press: KeyPress, width: usize) {
        match press.key {
            Key::Up if plain(&press) => self.move_up(width),
            Key::Down if plain(&press) => self.move_down(width),
            Key::Home if plain(&press) => self.move_home(width),
            Key::End if plain(&press) => self.move_end(width),
            _ => {
                self.textarea.input(to_input(press));
                self.highlighter.update(self.textarea.lines());
                // An edit or horizontal move abandons the remembered goal column
                // and reads a wrap boundary as the start of the following row.
                self.goal_col = None;
                self.bias = CursorBias::Forward;
            }
        }
    }

    /// Move up one visual row, keeping the remembered goal column. On the top
    /// visual row this leaves the cursor where it is.
    fn move_up(&mut self, width: usize) {
        let map = self.wrap(width);
        let (crow, ccol) = self.textarea.cursor();
        let (cvrow, cvcol) = map.cursor_to_visual(crow, ccol, self.bias);
        if cvrow == 0 {
            return;
        }
        let goal = *self.goal_col.get_or_insert(cvcol);
        let (row, col, bias) = map.vertical_target(cvrow - 1, goal);
        self.jump(row, col);
        self.bias = bias;
    }

    /// Move down one visual row, keeping the remembered goal column. On the
    /// bottom visual row this leaves the cursor where it is.
    fn move_down(&mut self, width: usize) {
        let map = self.wrap(width);
        let (crow, ccol) = self.textarea.cursor();
        let (cvrow, cvcol) = map.cursor_to_visual(crow, ccol, self.bias);
        if cvrow + 1 >= map.row_count() {
            return;
        }
        let goal = *self.goal_col.get_or_insert(cvcol);
        let (row, col, bias) = map.vertical_target(cvrow + 1, goal);
        self.jump(row, col);
        self.bias = bias;
    }

    /// Move the cursor to the start of its current visual row.
    fn move_home(&mut self, width: usize) {
        let map = self.wrap(width);
        let (crow, ccol) = self.textarea.cursor();
        let (cvrow, _) = map.cursor_to_visual(crow, ccol, self.bias);
        let (row, col) = map.row_start_cursor(cvrow);
        self.jump(row, col);
        self.goal_col = None;
        self.bias = CursorBias::Forward;
    }

    /// Move the cursor to the end of its current visual row.
    fn move_end(&mut self, width: usize) {
        let map = self.wrap(width);
        let (crow, ccol) = self.textarea.cursor();
        let (cvrow, _) = map.cursor_to_visual(crow, ccol, self.bias);
        let (row, col) = map.row_end_cursor(cvrow);
        self.jump(row, col);
        self.goal_col = None;
        // End on a hard-wrapped row (no trailing space to trim) reaches the
        // boundary; bias it back onto this row rather than the next.
        self.bias = CursorBias::Backward;
    }

    /// Move the logical cursor to visual (row, col).
    fn jump(&mut self, row: usize, col: usize) {
        self.textarea.move_cursor(CursorMove::Jump(
            row.min(u16::MAX as usize) as u16,
            col.min(u16::MAX as usize) as u16,
        ));
    }

    /// The wrapped view of the editor's contents at `width` columns.
    fn wrap(&self, width: usize) -> WrapMap {
        WrapMap::build(self.textarea.lines(), width)
    }

    /// The view row the editor renders above.
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// The editor's border block, titled with the label and either the
    /// submit/cancel hint or the discard confirmation.
    pub fn block(&self) -> Block<'static> {
        let title = if self.confirming {
            format!(" {}  discard changes? y/n ", self.label)
        } else {
            format!(" {}  {} ", self.label, self.hint)
        };
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(color(self.border)))
            .title(title)
    }

    /// The visible interior of the editor, wrapped to `width` columns and shown
    /// at most `max_interior` rows tall, scrolled to keep the cursor in view.
    pub fn layout(&self, width: usize, max_interior: usize) -> EditorView {
        let map = self.wrap(width);
        let total = map.row_count();
        // Show no more rows than fit the cap, and never more than the body has.
        let window = max_interior.min(total).max(1);
        let (crow, ccol) = self.textarea.cursor();
        let (cvrow, cvcol) = map.cursor_to_visual(crow, ccol, self.bias);
        let offset = scroll_top(cvrow, window, total);
        let last = (offset + window).min(total);
        let rows = (offset..last)
            .map(|vrow| self.styled_row(map.row(vrow)))
            .collect();
        let cursor = (offset..last).contains(&cvrow).then(|| {
            (
                cvcol.min(u16::MAX as usize) as u16,
                (cvrow - offset).min(u16::MAX as usize) as u16,
            )
        });
        let scroll = (total > window).then_some(Scroll { offset, total });
        EditorView {
            rows,
            cursor,
            scroll,
        }
    }

    // Colors the chars visual row `row` covers with the cached markdown spans of
    // its logical line.
    fn styled_row(&self, row: &VisualRow) -> Line<'static> {
        let mut out = Vec::new();
        let mut col = 0usize;
        for span in self.highlighter.line_spans(row.logical) {
            let span_start = col;
            col += span.text.chars().count();
            let lo = row.start_char.max(span_start);
            let hi = row.end_char.min(col);
            if lo < hi {
                let text = char_slice(&span.text, lo - span_start, hi - span_start);
                out.push(Span::styled(text.to_string(), style_of(&span.style)));
            }
            if col >= row.end_char {
                break;
            }
        }
        Line::from(out)
    }

    /// The current body text, trailing blank lines trimmed.
    pub fn body(&self) -> String {
        self.textarea.lines().join("\n").trim_end().to_string()
    }

    /// Whether the body differs from the text the editor opened with.
    pub fn is_dirty(&self) -> bool {
        self.textarea.lines().join("\n") != self.original
    }

    /// What this editor will produce when saved.
    pub fn into_kind(self) -> ComposeKind {
        self.kind
    }

    /// The id of the comment this editor revises, or `None` when it authors a
    /// new one. The review hides the rendered form of this comment while it is
    /// being edited, so the editor stands in its place.
    pub fn editing(&self) -> Option<Ulid> {
        match self.kind {
            ComposeKind::Edit(id) => Some(id),
            ComposeKind::Add(_) => None,
        }
    }

    /// Whether a cancel is awaiting confirmation.
    pub fn confirming(&self) -> bool {
        self.confirming
    }

    /// Begin confirming a cancel, updating the border to ask for an answer.
    pub fn begin_confirm(&mut self) {
        self.confirming = true;
    }

    /// Return to editing after a declined cancel, restoring the border.
    pub fn resume(&mut self) {
        self.confirming = false;
    }
}

/// The scroll offset that keeps visual row `cursor` inside a `window`-tall
/// viewport over `total` rows, clamped so no blank strip trails the last row.
fn scroll_top(cursor: usize, window: usize, total: usize) -> usize {
    let scroll = if cursor < window {
        0
    } else {
        cursor + 1 - window
    };
    scroll.min(total.saturating_sub(window))
}

/// Whether a press has no modifiers, so a motion key acts on the editor rather
/// than being reserved for a modified binding.
fn plain(press: &KeyPress) -> bool {
    !press.ctrl && !press.alt && !press.shift
}

/// Substring of `s` spanning char indices `start..end`.
fn char_slice(s: &str, start: usize, end: usize) -> &str {
    let byte = |char_index: usize| s.char_indices().nth(char_index).map_or(s.len(), |(i, _)| i);
    &s[byte(start)..byte(end)]
}

fn style_of(style: &wiff_diff::Style) -> Style {
    let mut out = Style::default().fg(color(style.fg));
    if style.bold {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Translate a binding [`KeyPress`] into the editor's own input type, so the
/// text buffer handles motion and editing without seeing wiff's key model. An
/// insert key has no editor action and maps to the editor's null input.
fn to_input(press: KeyPress) -> Input {
    let key = match press.key {
        Key::Char(c) => EditorKey::Char(c),
        Key::Enter => EditorKey::Enter,
        Key::Backspace => EditorKey::Backspace,
        Key::Delete => EditorKey::Delete,
        Key::Tab => EditorKey::Tab,
        Key::Left => EditorKey::Left,
        Key::Right => EditorKey::Right,
        Key::Up => EditorKey::Up,
        Key::Down => EditorKey::Down,
        Key::Home => EditorKey::Home,
        Key::End => EditorKey::End,
        Key::PageUp => EditorKey::PageUp,
        Key::PageDown => EditorKey::PageDown,
        Key::Function(n) => EditorKey::F(n),
        Key::Escape => EditorKey::Esc,
        Key::Insert => EditorKey::Null,
    };
    Input {
        key,
        ctrl: press.ctrl,
        alt: press.alt,
        shift: press.shift,
    }
}
