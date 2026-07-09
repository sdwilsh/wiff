//! Buffered, uncommitted review edits.
//!
//! The TUI edits a review in memory before writing anything: adding, editing,
//! resolving, and deleting comments accumulate as **drafts** and reach the
//! session log only when the reviewer commits. A [`DraftBuffer`] holds those
//! pending changes, layers them over the committed comments folded from the log
//! to produce the effective comment list a renderer draws (each flagged whether
//! it has an uncommitted change), and on commit yields the append events that
//! persist them, in the order they were made.
//!
//! A drafted comment carries the same anchor data a committed one does, so a
//! refresh rebases pending line-range comments forward alongside persisted ones.
//!
//! The buffer trusts its caller: an edit, resolve, or delete names a comment
//! that is present in the current effective set, either a committed comment or a
//! drafted addition. It keeps itself minimal as edits pile up, folding repeated
//! changes to one comment together and dropping a drafted addition entirely when
//! it is deleted before ever being committed.

use ulid::Ulid;

use crate::record::{
    CommentDelete, CommentEdit, CommentRecord, CommentResolve, CommentTarget, RecordBody,
};
use crate::review::CommentState;

/// One buffered change to the review.
#[derive(Debug, Clone, PartialEq)]
enum DraftOp {
    /// A newly authored comment.
    Add(CommentRecord),
    /// A revision to a comment's body.
    Edit { id: Ulid, body: String },
    /// A change to a comment's resolved state.
    Resolve { id: Ulid, resolved: bool },
    /// A withdrawal of a comment.
    Delete { id: Ulid },
}

impl DraftOp {
    /// The comment this change applies to.
    fn id(&self) -> Ulid {
        match self {
            DraftOp::Add(record) => record.id,
            DraftOp::Edit { id, .. } | DraftOp::Resolve { id, .. } | DraftOp::Delete { id } => *id,
        }
    }
}

/// A comment as it currently stands with pending drafts applied.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveComment {
    /// The comment's current state.
    pub comment: CommentState,
    /// Whether it has an uncommitted change (a draft badge is shown for it).
    pub pending: bool,
}

/// The pending drafts against a review, applied over its committed comments.
#[derive(Debug, Clone, Default)]
pub struct DraftBuffer {
    ops: Vec<DraftOp>,
}

impl DraftBuffer {
    /// An empty buffer with no pending drafts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there are no pending drafts.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Buffer a newly authored comment, returning its identity so later edits
    /// can name it. The record's anchor and authored-against version are kept so
    /// a refresh can rebase the draft before it is ever committed.
    pub fn add(&mut self, record: CommentRecord) -> Ulid {
        let id = record.id;
        self.ops.push(DraftOp::Add(record));
        id
    }

    /// Buffer a new body for comment `id`. Editing a drafted addition rewrites
    /// that draft in place; editing a committed comment replaces any earlier
    /// buffered edit so only the latest body is committed.
    pub fn edit(&mut self, id: Ulid, body: String) {
        for op in &mut self.ops {
            if let DraftOp::Add(record) = op
                && record.id == id
            {
                record.body = body;
                return;
            }
        }
        self.ops
            .retain(|op| !matches!(op, DraftOp::Edit { id: other, .. } if *other == id));
        self.ops.push(DraftOp::Edit { id, body });
    }

    /// Buffer a resolved-state change for comment `id`, replacing any earlier
    /// buffered resolve so only the latest state is committed.
    pub fn resolve(&mut self, id: Ulid, resolved: bool) {
        self.ops
            .retain(|op| !matches!(op, DraftOp::Resolve { id: other, .. } if *other == id));
        self.ops.push(DraftOp::Resolve { id, resolved });
    }

    /// Buffer a withdrawal of comment `id`. Deleting a drafted addition that was
    /// never committed discards it and its buffered edits outright; deleting a
    /// committed comment drops its buffered edits and records a tombstone.
    pub fn delete(&mut self, id: Ulid) {
        let was_drafted = self
            .ops
            .iter()
            .any(|op| matches!(op, DraftOp::Add(record) if record.id == id));
        self.ops.retain(|op| op.id() != id);
        if !was_drafted {
            self.ops.push(DraftOp::Delete { id });
        }
    }

    /// The effective comments after applying the pending drafts over
    /// `committed`: committed comments in their existing order with edits,
    /// resolves, and deletes folded in, then drafted additions in the order they
    /// were made. Each is flagged whether it carries an uncommitted change.
    pub fn apply(&self, committed: &[CommentState]) -> Vec<EffectiveComment> {
        let mut order: Vec<Ulid> = committed.iter().map(|comment| comment.id).collect();
        let mut states: Vec<(Ulid, CommentState)> = committed
            .iter()
            .map(|comment| (comment.id, comment.clone()))
            .collect();
        for op in &self.ops {
            match op {
                DraftOp::Add(record) => {
                    order.push(record.id);
                    states.push((record.id, CommentState::created(record, 0)));
                }
                DraftOp::Edit { id, body } => {
                    if let Some(state) = find_mut(&mut states, *id) {
                        state.body = body.clone();
                    }
                }
                DraftOp::Resolve { id, resolved } => {
                    if let Some(state) = find_mut(&mut states, *id) {
                        state.resolved = *resolved;
                    }
                }
                DraftOp::Delete { id } => {
                    if let Some(state) = find_mut(&mut states, *id) {
                        state.deleted = true;
                    }
                }
            }
        }
        let pending: Vec<Ulid> = self.ops.iter().map(DraftOp::id).collect();
        order
            .into_iter()
            .filter_map(|id| find_mut(&mut states, id).map(|state| (id, state.clone())))
            .filter(|(_, state)| !state.deleted)
            .map(|(id, comment)| EffectiveComment {
                pending: pending.contains(&id),
                comment,
            })
            .collect()
    }

    /// The append events that persist the pending drafts, in the order they were
    /// made. Consumed because committing empties the buffer.
    pub fn into_records(self) -> Vec<RecordBody> {
        self.ops
            .into_iter()
            .map(|op| match op {
                DraftOp::Add(record) => RecordBody::Comment(record),
                DraftOp::Edit { id, body } => RecordBody::CommentEdit(CommentEdit { id, body }),
                DraftOp::Resolve { id, resolved } => {
                    RecordBody::CommentResolve(CommentResolve { id, resolved })
                }
                DraftOp::Delete { id } => RecordBody::CommentDelete(CommentDelete { id }),
            })
            .collect()
    }
}

/// A fresh comment record for a body drafted against diff `version`, with the
/// caller-supplied `target` and `anchor`, assigned a new identity.
pub fn draft_record(
    author: crate::record::Author,
    target: CommentTarget,
    version: u32,
    anchor: Option<crate::record::Anchor>,
    body: String,
) -> CommentRecord {
    CommentRecord {
        id: Ulid::new(),
        author,
        target,
        version,
        anchor,
        body,
    }
}

/// The state of the comment with `id` among `states`, if present.
fn find_mut(states: &mut [(Ulid, CommentState)], id: Ulid) -> Option<&mut CommentState> {
    states
        .iter_mut()
        .find(|(other, _)| *other == id)
        .map(|(_, state)| state)
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::{DraftBuffer, EffectiveComment, draft_record};
    use crate::record::{Author, AuthorKind, CommentTarget, RecordBody};
    use crate::review::CommentState;

    /// A committed review comment with the given identity, body, and resolved
    /// state, targeting the review overall.
    fn committed_comment(id: u128, body: &str, resolved: bool) -> CommentState {
        CommentState {
            id: Ulid(id),
            author: Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            target: CommentTarget::Review,
            version: 0,
            anchor: None,
            body: body.to_string(),
            resolved,
            deleted: false,
            confidence: None,
            created_seq: 3,
            updated_seq: 3,
        }
    }

    /// A drafted review-level comment record by an agent with the given identity
    /// and body.
    fn drafted(id: u128, body: &str) -> crate::record::CommentRecord {
        let mut record = draft_record(
            Author {
                name: "opus".to_string(),
                kind: AuthorKind::Agent,
            },
            CommentTarget::Review,
            0,
            None,
            body.to_string(),
        );
        record.id = Ulid(id);
        record
    }

    #[test]
    fn an_empty_buffer_leaves_the_committed_comments_untouched() {
        let committed = vec![committed_comment(1, "looks fine", false)];
        let buffer = DraftBuffer::new();
        k9::assert_equal!(buffer.is_empty(), true);
        k9::assert_equal!(
            buffer.apply(&committed),
            vec![EffectiveComment {
                comment: committed_comment(1, "looks fine", false),
                pending: false,
            }]
        );
    }

    #[test]
    fn a_drafted_comment_appears_after_the_committed_ones_marked_pending() {
        let committed = vec![committed_comment(1, "looks fine", false)];
        let mut buffer = DraftBuffer::new();
        buffer.add(drafted(2, "one more thing"));
        let mut expected_new = CommentState {
            author: Author {
                name: "opus".to_string(),
                kind: AuthorKind::Agent,
            },
            ..committed_comment(2, "one more thing", false)
        };
        expected_new.created_seq = 0;
        expected_new.updated_seq = 0;
        k9::assert_equal!(
            buffer.apply(&committed),
            vec![
                EffectiveComment {
                    comment: committed_comment(1, "looks fine", false),
                    pending: false,
                },
                EffectiveComment {
                    comment: expected_new,
                    pending: true,
                },
            ]
        );
    }

    #[test]
    fn editing_and_resolving_a_committed_comment_marks_it_pending() {
        let committed = vec![committed_comment(1, "old body", false)];
        let mut buffer = DraftBuffer::new();
        buffer.edit(Ulid(1), "new body".to_string());
        buffer.resolve(Ulid(1), true);
        let mut expected = committed_comment(1, "new body", true);
        expected.created_seq = 3;
        expected.updated_seq = 3;
        k9::assert_equal!(
            buffer.apply(&committed),
            vec![EffectiveComment {
                comment: expected,
                pending: true,
            }]
        );
    }

    #[test]
    fn deleting_a_committed_comment_drops_it_from_the_effective_view() {
        let committed = vec![
            committed_comment(1, "a", false),
            committed_comment(2, "b", false),
        ];
        let mut buffer = DraftBuffer::new();
        buffer.delete(Ulid(1));
        k9::assert_equal!(
            buffer.apply(&committed),
            vec![EffectiveComment {
                comment: committed_comment(2, "b", false),
                pending: false,
            }]
        );
    }

    #[test]
    fn deleting_a_drafted_comment_discards_it_and_leaves_no_records() {
        let mut buffer = DraftBuffer::new();
        let id = buffer.add(drafted(2, "never mind"));
        buffer.edit(id, "second thoughts".to_string());
        buffer.delete(id);
        k9::assert_equal!(buffer.is_empty(), true);
        k9::assert_equal!(buffer.apply(&[]), Vec::<EffectiveComment>::new());
        k9::assert_equal!(buffer.into_records(), Vec::<RecordBody>::new());
    }

    #[test]
    fn repeated_edits_to_one_comment_commit_as_a_single_event() {
        let mut buffer = DraftBuffer::new();
        buffer.edit(Ulid(1), "first".to_string());
        buffer.edit(Ulid(1), "final".to_string());
        k9::assert_equal!(
            buffer.into_records(),
            vec![RecordBody::CommentEdit(crate::record::CommentEdit {
                id: Ulid(1),
                body: "final".to_string(),
            })]
        );
    }

    #[test]
    fn commit_emits_the_buffered_events_in_order() {
        let mut buffer = DraftBuffer::new();
        buffer.add(drafted(2, "new comment"));
        buffer.resolve(Ulid(1), true);
        k9::assert_equal!(
            buffer.into_records(),
            vec![
                RecordBody::Comment(drafted(2, "new comment")),
                RecordBody::CommentResolve(crate::record::CommentResolve {
                    id: Ulid(1),
                    resolved: true,
                }),
            ]
        );
    }
}
