//! The review being edited: the diff, its committed comments, and the buffered
//! draft edits layered over them.
//!
//! A [`Review`] pairs the renderer and its inputs with a [`DraftBuffer`], so an
//! edit made in the TUI folds into the buffer and the next [`document`](Review::document)
//! reflects it without touching the session log. Rendering applies the drafts
//! over the committed comments and badges the ones that carry an uncommitted
//! change, so pending work is visible until it is committed.

use std::collections::{HashMap, HashSet};

use ulid::Ulid;
use wiff_core::draft::{DraftBuffer, draft_record};
use wiff_core::record::{Author, CommentTarget, RecordBody};
use wiff_core::review::CommentState;
use wiff_diff::Diff;

use crate::render::{DiffView, Document, FileHighlights, ViewLayout};

/// How a reload of committed comments differed from the set already shown, so a
/// reviewer can be told what another actor changed while they were reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommentSync {
    /// Comments that appeared that were not shown before.
    pub added: usize,
    /// Comments shown before whose content changed.
    pub changed: usize,
    /// Comments shown before that are gone, withdrawn by another actor.
    pub removed: usize,
}

impl CommentSync {
    /// Whether nothing changed, so there is nothing to report.
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.changed == 0 && self.removed == 0
    }

    /// Compare the previously shown comments to the freshly loaded set, matching
    /// by identity and counting a comment as changed when its latest event moved.
    fn between(before: &[CommentState], after: &[CommentState]) -> Self {
        let seen: HashMap<Ulid, u64> = before.iter().map(|c| (c.id, c.updated_seq)).collect();
        let kept: HashSet<Ulid> = after.iter().map(|c| c.id).collect();
        let mut sync = CommentSync::default();
        for comment in after {
            match seen.get(&comment.id) {
                None => sync.added += 1,
                Some(&updated_seq) if updated_seq != comment.updated_seq => sync.changed += 1,
                Some(_) => {}
            }
        }
        sync.removed = before.iter().filter(|c| !kept.contains(&c.id)).count();
        sync
    }
}

/// A diff under review together with its committed comments and the buffered
/// edits layered over them.
pub struct Review {
    view: DiffView,
    diff: Diff,
    /// The diff's syntax highlighting, computed once per diff so a comment edit
    /// re-renders without re-running the highlighter. Refreshed alongside the
    /// diff.
    highlights: Vec<FileHighlights>,
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
        let highlights = view.highlight(&diff);
        Self {
            view,
            diff,
            highlights,
            author,
            version,
            committed: comments,
            drafts: DraftBuffer::new(),
        }
    }

    /// Render the effective review -- the committed comments with the pending
    /// drafts applied -- into a fresh document, badging the comments that carry
    /// an uncommitted change. Comment bodies wrap to `layout`, and the diff
    /// content wraps too when the layout asks for it; a zero-width layout leaves
    /// everything unwrapped, for use before a real width is known.
    pub fn document(&self, layout: ViewLayout) -> Document {
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
        self.view
            .render_review_cached(&self.diff, &comments, &pending, &self.highlights, layout)
    }

    /// Flip the resolved state of comment `id`, buffering the change.
    pub fn toggle_resolved(&mut self, id: Ulid) {
        let resolved = self
            .drafts
            .apply(&self.committed)
            .into_iter()
            .find(|entry| entry.comment.id == id)
            .is_some_and(|entry| entry.comment.resolved);
        self.drafts.resolve(id, !resolved, self.author.clone());
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
            self.drafts.delete(id, self.author.clone());
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

    /// How many effective comments are still open: neither resolved nor
    /// withdrawn, with pending drafts applied so the tally follows the
    /// reviewer's uncommitted edits.
    pub fn open_comments(&self) -> usize {
        self.drafts
            .apply(&self.committed)
            .into_iter()
            .filter(|entry| !entry.comment.resolved && !entry.comment.deleted)
            .count()
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
        self.highlights = self.view.highlight(&diff);
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

    /// Replace the committed comments with `comments`, the freshly folded live
    /// set after the pending drafts were persisted. The caller has already
    /// taken the drafts, so the review now reflects them as committed.
    pub fn set_committed(&mut self, comments: Vec<CommentState>) -> CommentSync {
        let sync = CommentSync::between(&self.committed, &comments);
        self.committed = comments;
        sync
    }
}

#[cfg(test)]
mod tests {
    use wiff_core::record::{Author, AuthorKind, CommentTarget};

    use super::{CommentState, CommentSync};

    /// A committed comment with identity `id` whose most recent event is `seq`,
    /// the only fields the sync tally compares.
    fn committed(id: u128, updated_seq: u64) -> CommentState {
        CommentState {
            id: ulid::Ulid(id),
            author: Author {
                name: "agent".to_string(),
                kind: AuthorKind::Agent,
            },
            target: CommentTarget::Review,
            version: 0,
            anchor: None,
            body: "note".to_string(),
            resolved: false,
            resolved_by: None,
            deleted: false,
            deleted_by: None,
            confidence: None,
            created_seq: updated_seq,
            updated_seq,
        }
    }

    #[test]
    fn a_sync_tally_counts_added_changed_and_removed_comments() {
        // Against a set holding comments 1 and 2, a reload that keeps 1 as it
        // was, advances 2 to a later event, and introduces 3 counts one added
        // and one changed; comment 1 is unchanged and comment 2 is not removed.
        let before = vec![committed(1, 5), committed(2, 5)];
        let after = vec![committed(1, 5), committed(2, 9), committed(3, 1)];
        k9::assert_equal!(
            CommentSync::between(&before, &after),
            CommentSync {
                added: 1,
                changed: 1,
                removed: 0,
            }
        );

        // Dropping comment 2 from the reloaded set counts as one removed, and an
        // identical reload reports nothing at all.
        k9::assert_equal!(
            CommentSync::between(&before, &[committed(1, 5)]),
            CommentSync {
                added: 0,
                changed: 0,
                removed: 1,
            }
        );
        k9::assert_equal!(CommentSync::between(&before, &before).is_empty(), true);
    }
}
