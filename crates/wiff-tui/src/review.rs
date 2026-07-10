//! The review being edited: the diff, its committed comments, and the buffered
//! draft edits layered over them.
//!
//! A [`Review`] pairs the renderer and its inputs with a [`DraftBuffer`], so an
//! edit made in the TUI folds into the buffer and the next [`document`](Review::document)
//! reflects it without touching the session log. Rendering applies the drafts
//! over the committed comments and badges the ones that carry an uncommitted
//! change, so pending work is visible until it is committed.

use ulid::Ulid;
use wiff_core::draft::DraftBuffer;
use wiff_core::record::RecordBody;
use wiff_core::review::CommentState;
use wiff_diff::Diff;

use crate::render::{DiffView, Document};

/// A diff under review together with its committed comments and the buffered
/// edits layered over them.
pub struct Review {
    view: DiffView,
    diff: Diff,
    /// The live comments already persisted to the session log.
    committed: Vec<CommentState>,
    /// The buffered, uncommitted edits over the committed comments.
    drafts: DraftBuffer,
}

impl Review {
    /// A review over `diff` with its already-committed `comments`, and no
    /// pending drafts. The caller filters out withdrawn committed comments.
    pub fn new(view: DiffView, diff: Diff, comments: Vec<CommentState>) -> Self {
        Self {
            view,
            diff,
            committed: comments,
            drafts: DraftBuffer::new(),
        }
    }

    /// Render the effective review -- the committed comments with the pending
    /// drafts applied -- into a fresh document, badging the comments that carry
    /// an uncommitted change.
    pub fn document(&self) -> Document {
        let effective = self.drafts.apply(&self.committed);
        let comments: Vec<CommentState> = effective
            .iter()
            .map(|entry| entry.comment.clone())
            .collect();
        let pending: Vec<Ulid> = effective
            .iter()
            .filter(|entry| entry.pending)
            .map(|entry| entry.comment.id)
            .collect();
        self.view.render_review(&self.diff, &comments, &pending)
    }

    /// Flip the resolved state of comment `id`, buffering the change.
    pub fn toggle_resolved(&mut self, id: Ulid) {
        let resolved = self
            .drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
            .is_some_and(|entry| entry.comment.resolved);
        self.drafts.resolve(id, !resolved);
    }

    /// Toggle the deleted state of comment `id`, buffering the change, and
    /// return whether it is now deleted. A deleted comment stays in the review
    /// shown as withdrawn until it is committed or restored.
    pub fn toggle_deleted(&mut self, id: Ulid) -> bool {
        let deleted = self
            .drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
            .is_some_and(|entry| entry.comment.deleted);
        if deleted {
            self.drafts.restore(id);
        } else {
            self.drafts.delete(id);
        }
        !deleted
    }

    /// Take the buffered drafts as the append events that persist them, emptying
    /// the buffer. Used at commit time when the reviewer keeps the session.
    pub fn take_drafts(&mut self) -> Vec<RecordBody> {
        std::mem::take(&mut self.drafts).into_records()
    }
}
