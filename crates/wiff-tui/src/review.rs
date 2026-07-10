//! The review being edited: the diff, its committed comments, and the buffered
//! draft edits layered over them.
//!
//! A [`Review`] pairs the renderer and its inputs with a [`DraftBuffer`], so an
//! edit made in the TUI folds into the buffer and the next [`document`](Review::document)
//! reflects it without touching the session log. Rendering applies the drafts
//! over the committed comments and badges the ones that carry an uncommitted
//! change, so pending work is visible until it is committed.

use ulid::Ulid;
use wiff_core::draft::{DraftBuffer, draft_record};
use wiff_core::record::{Author, CommentTarget, RecordBody};
use wiff_core::review::CommentState;
use wiff_diff::Diff;

use crate::render::{DiffView, Document};

/// A diff under review together with its committed comments and the buffered
/// edits layered over them.
pub struct Review {
    view: DiffView,
    diff: Diff,
    /// The author newly drafted comments are attributed to.
    author: Author,
    /// The diff version drafted comments are authored against.
    version: u32,
    /// The live comments already persisted to the session log.
    committed: Vec<CommentState>,
    /// The buffered, uncommitted edits over the committed comments.
    drafts: DraftBuffer,
}

impl Review {
    /// A review over `diff` with its already-committed `comments`, and no
    /// pending drafts. Comments authored in the TUI are attributed to `author`
    /// and anchored against diff `version`. The caller filters out withdrawn
    /// committed comments.
    pub fn new(
        view: DiffView,
        diff: Diff,
        author: Author,
        version: u32,
        comments: Vec<CommentState>,
    ) -> Self {
        Self {
            view,
            diff,
            author,
            version,
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

    /// Draft a new comment on `target` with `body`, returning its identity so
    /// the caller can focus it. The draft is anchored against the review's diff
    /// version and attributed to its author; snippet capture is left for a
    /// later refresh.
    pub fn add_comment(&mut self, target: CommentTarget, body: String) -> Ulid {
        let record = draft_record(self.author.clone(), target, self.version, None, body);
        self.drafts.add(record)
    }

    /// Buffer a new `body` for comment `id`.
    pub fn edit_comment(&mut self, id: Ulid, body: String) {
        self.drafts.edit(id, body);
    }

    /// The current body of comment `id` with pending drafts applied, for seeding
    /// an edit. Absent when no such comment is in the effective set.
    pub fn comment_body(&self, id: Ulid) -> Option<String> {
        self.drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
            .map(|entry| entry.comment.body)
    }

    /// Whether any uncommitted draft edits are buffered, so the reviewer is
    /// warned before leaving that leaving without committing loses them.
    pub fn has_drafts(&self) -> bool {
        !self.drafts.is_empty()
    }

    /// Recapture the review over `diff` as version `version`, replacing the diff
    /// and its committed `comments` and rebasing pending drafts forward onto it.
    /// A drafted line comment moves through `old_diff`, which yields the parsed
    /// diff a draft was authored against so its anchored lines can be relocated.
    pub fn refresh(
        &mut self,
        diff: Diff,
        comments: Vec<CommentState>,
        version: u32,
        old_diff: impl FnMut(u32) -> wiff_core::Result<Diff>,
    ) -> wiff_core::Result<()> {
        self.drafts.rebase(version, &diff, old_diff)?;
        self.diff = diff;
        self.committed = comments;
        self.version = version;
        Ok(())
    }

    /// Take the buffered drafts as the append events that persist them, emptying
    /// the buffer. Used at commit time when the reviewer keeps the session.
    pub fn take_drafts(&mut self) -> Vec<RecordBody> {
        std::mem::take(&mut self.drafts).into_records()
    }
}
