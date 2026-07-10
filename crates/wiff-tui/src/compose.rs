//! The inline editor for authoring or revising a comment body.
//!
//! A [`Compose`] wraps a [`TextArea`] shown in place of the comment it will
//! become: authoring one anchors it above the line it targets, revising one
//! anchors it above the comment's header. The editor owns its own keys (cursor
//! motion, word and line kills, undo); the app hands it every press except the
//! two that leave it, save and cancel. Cancelling after the body has changed
//! asks for confirmation first so an accidental keystroke cannot discard work.

use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};
use tui_textarea::{CursorMove, Input, Key as EditorKey, TextArea};
use ulid::Ulid;
use wiff_core::record::CommentTarget;
use wiff_diff::Rgb;

use crate::key::{Key, KeyPress};
use crate::render::color;

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
    /// The color of the editor's border.
    border: Rgb,
    /// Whether a cancel with unsaved changes is awaiting a yes/no answer.
    confirming: bool,
}

impl Compose {
    /// An editor for `kind`, rendered above view row `anchor`, seeded with
    /// `seed` (empty when authoring), titled `label`, and bordered in `border`.
    pub fn new(kind: ComposeKind, anchor: usize, seed: &str, label: String, border: Rgb) -> Self {
        let lines: Vec<String> = if seed.is_empty() {
            vec![String::new()]
        } else {
            seed.split('\n').map(str::to_string).collect()
        };
        let mut textarea = TextArea::new(lines);
        // The editor otherwise underlines the whole line the cursor is on, which
        // reads as emphasis on the text rather than a cursor.
        textarea.set_cursor_line_style(Style::default());
        // Open with the cursor after the seeded text so an edit appends.
        textarea.move_cursor(CursorMove::Bottom);
        textarea.move_cursor(CursorMove::End);
        let mut compose = Self {
            textarea,
            kind,
            anchor,
            original: seed.to_string(),
            label,
            border,
            confirming: false,
        };
        compose.apply_block();
        compose
    }

    /// Feed a key press to the text buffer.
    pub fn input(&mut self, press: KeyPress) {
        self.textarea.input(to_input(press));
    }

    /// The view row the editor renders above.
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// The live editor widget.
    pub fn editor(&self) -> &TextArea<'static> {
        &self.textarea
    }

    /// The number of body lines, for sizing the editor box.
    pub fn line_count(&self) -> usize {
        self.textarea.lines().len()
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
        self.apply_block();
    }

    /// Return to editing after a declined cancel, restoring the border.
    pub fn resume(&mut self) {
        self.confirming = false;
        self.apply_block();
    }

    /// Set the editor's border and title from its current state: the label plus
    /// either the save/cancel hint or the discard confirmation.
    fn apply_block(&mut self) {
        let title = if self.confirming {
            format!(" {}  discard changes? y/n ", self.label)
        } else {
            format!(" {}  ctrl-s save  esc cancel ", self.label)
        };
        self.textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color(self.border)))
                .title(title),
        );
    }
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
