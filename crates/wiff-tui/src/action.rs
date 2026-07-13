//! The action vocabulary.
//!
//! Input events are decoded into an [`Action`] before the UI reacts, so the UI
//! logic branches on intent rather than on raw keys. This keeps key bindings
//! reassignable and the update logic small. Action names double as their config
//! identifiers via [`Action::name`].

use serde::{Deserialize, Serialize};

/// A single reviewer intent the UI can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    /// Move to the next comment.
    NextComment,
    /// Move to the previous comment.
    PrevComment,
    /// Expand or collapse the fold at the cursor.
    ToggleFold,
    /// Expand or collapse the comment at the cursor.
    ToggleComment,
    /// Toggle whether diff content wraps to the viewport width or is clipped.
    ToggleWrap,
    /// Toggle whether comments are shown at all, so the code reads without the
    /// annotations in the way.
    HideComments,
    /// Open the modal list of files to jump to one.
    PickFile,
    /// Open the modal list of comments to jump to one.
    PickComment,
    /// Open the modal list of color themes to switch to one.
    PickTheme,
    /// Start a linewise selection for anchoring a comment to a range, seeding
    /// from a draft comment's range when the cursor is on one.
    SelectLines,
    /// Add a comment at the cursor.
    AddComment,
    /// Edit the focused comment.
    EditComment,
    /// Toggle the focused comment's resolved state.
    ResolveComment,
    /// Delete the focused comment, or restore it when already deleted.
    DeleteComment,
    /// Confirm the comment being composed, moving it into the pending drafts.
    /// Only acts while the inline editor is open.
    SubmitComment,
    /// Abandon the comment being composed, closing the inline editor. Only acts
    /// while the editor is open, confirming first when the body has changed.
    CancelComment,
    /// Detach the open editor from its anchor, floating it at a screen edge and
    /// freeing the cursor to navigate the diff for something to reference. Only
    /// acts while the editor is open.
    DetachEditor,
    /// Commit the pending draft edits to the session log, keeping the review
    /// open.
    Save,
    /// Open the incremental search prompt, scanning forward.
    SearchForward,
    /// Open the incremental search prompt, scanning backward.
    SearchBackward,
    /// Move to the next match of the last search.
    SearchNext,
    /// Move to the previous match of the last search.
    SearchPrev,
    /// Capture a new diff version and rebase comments.
    Refresh,
    /// Open the modal list of captured versions to compare the review against
    /// an earlier one.
    CompareVersions,
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
            Action::NextComment => "next_comment",
            Action::PrevComment => "prev_comment",
            Action::ToggleFold => "toggle_fold",
            Action::ToggleComment => "toggle_comment",
            Action::ToggleWrap => "toggle_wrap",
            Action::HideComments => "hide_comments",
            Action::PickFile => "pick_file",
            Action::PickComment => "pick_comment",
            Action::PickTheme => "pick_theme",
            Action::SelectLines => "select_lines",
            Action::AddComment => "add_comment",
            Action::EditComment => "edit_comment",
            Action::ResolveComment => "resolve_comment",
            Action::DeleteComment => "delete_comment",
            Action::SubmitComment => "submit_comment",
            Action::CancelComment => "cancel_comment",
            Action::DetachEditor => "detach_editor",
            Action::Save => "save",
            Action::SearchForward => "search_forward",
            Action::SearchBackward => "search_backward",
            Action::SearchNext => "search_next",
            Action::SearchPrev => "search_prev",
            Action::Refresh => "refresh",
            Action::CompareVersions => "compare_versions",
            Action::OpenInEditor => "open_in_editor",
            Action::Quit => "quit",
            Action::QuitKeep => "quit_keep",
            Action::QuitRemove => "quit_remove",
        }
    }
}
