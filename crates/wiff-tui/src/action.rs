//! The action vocabulary.
//!
//! Input events are decoded into an [`Action`] before the UI reacts, so the UI
//! logic branches on intent rather than on raw keys. This keeps key bindings
//! reassignable and the update logic small. Action names double as their config
//! identifiers via [`Action::name`].

use serde::{Deserialize, Serialize};

/// A single reviewer intent the UI can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Move the cursor down one line.
    LineDown,
    /// Move the cursor up one line.
    LineUp,
    /// Scroll down one page.
    PageDown,
    /// Scroll up one page.
    PageUp,
    /// Jump to the start of the diff.
    Top,
    /// Jump to the end of the diff.
    Bottom,
    /// Move to the next file.
    NextFile,
    /// Move to the previous file.
    PrevFile,
    /// Move to the next hunk.
    NextHunk,
    /// Move to the previous hunk.
    PrevHunk,
    /// Add a comment at the cursor.
    AddComment,
    /// Edit the focused comment.
    EditComment,
    /// Toggle the focused comment's resolved state.
    ResolveComment,
    /// Delete the focused comment.
    DeleteComment,
    /// Capture a new diff version and rebase comments.
    Refresh,
    /// Open the focused file in the user's editor.
    OpenInEditor,
    /// Quit, honoring the configured keep-or-remove default.
    Quit,
    /// Quit and keep the session for later resumption.
    QuitKeep,
    /// Quit and remove the session.
    QuitRemove,
}

impl Action {
    /// The action's stable snake_case identifier, used as its key in config.
    pub fn name(self) -> &'static str {
        match self {
            Action::LineDown => "line_down",
            Action::LineUp => "line_up",
            Action::PageDown => "page_down",
            Action::PageUp => "page_up",
            Action::Top => "top",
            Action::Bottom => "bottom",
            Action::NextFile => "next_file",
            Action::PrevFile => "prev_file",
            Action::NextHunk => "next_hunk",
            Action::PrevHunk => "prev_hunk",
            Action::AddComment => "add_comment",
            Action::EditComment => "edit_comment",
            Action::ResolveComment => "resolve_comment",
            Action::DeleteComment => "delete_comment",
            Action::Refresh => "refresh",
            Action::OpenInEditor => "open_in_editor",
            Action::Quit => "quit",
            Action::QuitKeep => "quit_keep",
            Action::QuitRemove => "quit_remove",
        }
    }
}
