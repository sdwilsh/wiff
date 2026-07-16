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
//! The buffer trusts its caller: an edit, resolve, delete, or restore names a
//! comment that is present in the current effective set, either a committed
//! comment or a drafted addition. It keeps itself minimal as edits pile up,
//! folding repeated changes to one comment together. A deletion is reversible
//! until commit, so a deleted comment stays buffered rather than being discarded;
//! at commit time a drafted addition that is still deleted, and any edits folded
//! into a comment that ends deleted, are pruned so they never reach the log.

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_diff::Diff;

use crate::comment::{delete_event, disposition_event, edit_event, resolve_event};
use crate::description::local_description;
use crate::error::Result;
use crate::rebase::rebase_line_comment;
use crate::record::{
    Anchor, Author, CommentCreate, CommentEvent, CommentEventKind, CommentTarget, Description,
    Disposition, RecordBody, Seq, VersionNumber,
};
use crate::review::{CommentState, DescriptionState};

/// One buffered change to the review.
// `Add` (a full create event) is the large variant and the common one; the
// mutation variants are small. Boxing to equalize would heap-allocate the hot
// path, so the size spread is kept.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
enum DraftOp {
    /// A newly authored comment, as its create event.
    Add(CommentEvent),
    /// A revision to a comment's body, and who made it.
    Edit {
        id: Ulid,
        author: Author,
        body: String,
    },
    /// A change to a comment's resolved state, and who made it.
    Resolve {
        id: Ulid,
        resolved: bool,
        author: Author,
    },
    /// A withdrawal of a comment, and who made it.
    Delete { id: Ulid, author: Author },
    /// A change to a comment's verdict, and who made it.
    SetDisposition {
        id: Ulid,
        disposition: Option<Disposition>,
        author: Author,
    },
}

impl DraftOp {
    /// The comment this change applies to.
    fn id(&self) -> Ulid {
        match self {
            DraftOp::Add(event) => event.id,
            DraftOp::Edit { id, .. }
            | DraftOp::Resolve { id, .. }
            | DraftOp::Delete { id, .. }
            | DraftOp::SetDisposition { id, .. } => *id,
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

/// A newly authored revision of the description, held until commit.
#[derive(Debug, Clone, PartialEq)]
struct DescriptionDraft {
    author: Author,
    content: Description,
}

/// The description as it currently stands: the committed revision with any
/// buffered edit layered over it.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveDescription {
    /// The current title and body.
    pub content: Description,
    /// Who set the current revision.
    pub author: Author,
    /// Whether an uncommitted edit is buffered for it.
    pub pending: bool,
}

/// The pending drafts against a review, applied over its committed comments.
#[derive(Debug, Clone, Default)]
pub struct DraftBuffer {
    ops: Vec<DraftOp>,
    /// The buffered description edit, absent when none has been made.
    description: Option<DescriptionDraft>,
}

impl DraftBuffer {
    /// An empty buffer with no pending drafts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there are no pending drafts.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.description.is_none()
    }

    /// Buffer a new description `content` by `author`, replacing any earlier
    /// buffered edit so only the latest is committed.
    pub fn edit_description(&mut self, author: Author, content: Description) {
        self.description = Some(DescriptionDraft { author, content });
    }

    /// The description as it currently stands with the buffered edit applied over
    /// `committed`: the buffered edit when present, else the committed revision,
    /// else `None` when the review has no description at all.
    pub fn effective_description(
        &self,
        committed: Option<&DescriptionState>,
    ) -> Option<EffectiveDescription> {
        if let Some(draft) = &self.description {
            return Some(EffectiveDescription {
                content: draft.content.clone(),
                author: draft.author.clone(),
                pending: true,
            });
        }
        committed.map(|state| EffectiveDescription {
            content: state.content.clone(),
            author: state.author.clone(),
            pending: false,
        })
    }

    /// Buffer a newly authored comment, returning its identity so later edits
    /// can name it.
    pub fn add(&mut self, event: CommentEvent) -> Ulid {
        let id = event.id;
        self.ops.push(DraftOp::Add(event));
        id
    }

    /// Buffer a new body for comment `id` by `author`. Editing a drafted
    /// addition rewrites that draft in place; editing a committed comment
    /// replaces any earlier buffered edit so only the latest body is committed.
    pub fn edit(&mut self, id: Ulid, author: Author, body: String) {
        for op in &mut self.ops {
            if let DraftOp::Add(event) = op
                && event.id == id
                && let CommentEventKind::Create(create) = &mut event.kind
            {
                create.body = body;
                return;
            }
        }
        self.ops
            .retain(|op| !matches!(op, DraftOp::Edit { id: other, .. } if *other == id));
        self.ops.push(DraftOp::Edit { id, author, body });
    }

    /// Buffer a resolved-state change for comment `id` by `author`, replacing any
    /// earlier buffered resolve so only the latest state is committed.
    pub fn resolve(&mut self, id: Ulid, resolved: bool, author: Author) {
        self.ops
            .retain(|op| !matches!(op, DraftOp::Resolve { id: other, .. } if *other == id));
        self.ops.push(DraftOp::Resolve {
            id,
            resolved,
            author,
        });
    }

    /// Buffer a verdict change for comment `id` by `author`, replacing any
    /// earlier buffered verdict so only the latest is committed.
    pub fn set_disposition(&mut self, id: Ulid, disposition: Option<Disposition>, author: Author) {
        self.ops
            .retain(|op| !matches!(op, DraftOp::SetDisposition { id: other, .. } if *other == id));
        self.ops.push(DraftOp::SetDisposition {
            id,
            disposition,
            author,
        });
    }

    /// Buffer a withdrawal of comment `id`, marking it deleted until commit. The
    /// comment stays in the effective set; the withdrawal is undone with
    /// [`restore`](Self::restore). Deleting is idempotent.
    pub fn delete(&mut self, id: Ulid, author: Author) {
        if self.is_deleting(id) {
            return;
        }
        self.ops.push(DraftOp::Delete { id, author });
    }

    /// Undo a buffered withdrawal of comment `id`, bringing it back into the
    /// review. A drafted addition is restored whole since its authoring draft is
    /// kept alongside the deletion.
    pub fn restore(&mut self, id: Ulid) {
        self.ops
            .retain(|op| !matches!(op, DraftOp::Delete { id: other, .. } if *other == id));
    }

    /// Whether a withdrawal of comment `id` is currently buffered.
    fn is_deleting(&self, id: Ulid) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op, DraftOp::Delete { id: other, .. } if *other == id))
    }

    /// Rebase pending drafted line-range comments forward onto `new_diff`, the
    /// freshly captured version numbered `new_version`. Each drafted line comment
    /// moves through the same engine that relocates committed comments, reading
    /// the diff it was authored against through `old_diff`. Whole-file and review
    /// drafts are not tied to line content, so they keep their target and
    /// version, matching how committed comments of those kinds are left where
    /// they are by a refresh. Buffered edits, resolves, and deletes name comments
    /// by identity and so need no move.
    pub fn rebase(
        &mut self,
        new_version: VersionNumber,
        new_diff: &Diff,
        mut old_diff: impl FnMut(VersionNumber) -> Result<Diff>,
    ) -> Result<()> {
        for op in &mut self.ops {
            if let DraftOp::Add(event) = op
                && let CommentEventKind::Create(create) = &mut event.kind
                && matches!(create.target, CommentTarget::Lines { .. })
            {
                let old = old_diff(create.version)?;
                if let Some(rebased) =
                    rebase_line_comment(&create.target, create.anchor.as_ref(), &old, new_diff)
                {
                    create.target = rebased.target;
                    create.version = new_version;
                }
            }
        }
        Ok(())
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
                DraftOp::Add(event) => {
                    let CommentEventKind::Create(create) = &event.kind else {
                        // A buffered addition is only ever built by
                        // `draft_create`, which yields a Create; any other kind
                        // is a programming error, not a state to render.
                        unreachable!("a buffered addition is always a create event");
                    };
                    order.push(event.id);
                    // A draft has no authored time until it is committed (its
                    // real time is the record's `at`); the preview uses the
                    // epoch as a stable placeholder.
                    let at = event.authored_at.unwrap_or(OffsetDateTime::UNIX_EPOCH);
                    states.push((
                        event.id,
                        CommentState::from_create(event, create, at, Seq(0)),
                    ));
                }
                DraftOp::Edit { id, body, .. } => {
                    if let Some(state) = find_mut(&mut states, *id) {
                        state.body = body.clone();
                    }
                }
                DraftOp::Resolve {
                    id,
                    resolved,
                    author,
                } => {
                    if let Some(state) = find_mut(&mut states, *id) {
                        state.resolved = *resolved;
                        state.resolved_by = Some(author.clone());
                    }
                }
                DraftOp::Delete { id, author } => {
                    if let Some(state) = find_mut(&mut states, *id) {
                        state.deleted = true;
                        state.deleted_by = Some(author.clone());
                    }
                }
                DraftOp::SetDisposition {
                    id, disposition, ..
                } => {
                    if let Some(state) = find_mut(&mut states, *id) {
                        state.disposition = *disposition;
                    }
                }
            }
        }
        let pending: Vec<Ulid> = self.ops.iter().map(DraftOp::id).collect();
        order
            .into_iter()
            .filter_map(|id| find_mut(&mut states, id).map(|state| (id, state.clone())))
            // A buffered deletion keeps the comment in view (shown as deleted) so
            // it can be undone; a comment that arrived already withdrawn stays
            // hidden.
            .filter(|(id, state)| !state.deleted || pending.contains(id))
            .map(|(id, comment)| EffectiveComment {
                pending: pending.contains(&id),
                comment,
            })
            .collect()
    }

    /// The append events that persist the pending drafts, in the order they were
    /// made. Consumed because committing empties the buffer. A drafted addition
    /// that is still deleted, and edits or resolves folded into a comment that
    /// ends deleted, are pruned so only a tombstone for a committed comment and
    /// live authoring reach the log.
    pub fn into_records(self) -> Vec<RecordBody> {
        let description = self
            .description
            .map(|draft| local_description(draft.author, draft.content));
        let deleted: Vec<Ulid> = self
            .ops
            .iter()
            .filter_map(|op| match op {
                DraftOp::Delete { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        let added: Vec<Ulid> = self
            .ops
            .iter()
            .filter_map(|op| match op {
                DraftOp::Add(record) => Some(record.id),
                _ => None,
            })
            .collect();
        self.ops
            .into_iter()
            .filter(|op| {
                let id = op.id();
                if !deleted.contains(&id) {
                    return true;
                }
                // A drafted addition that ends deleted leaves nothing behind;
                // a committed comment that ends deleted keeps only its tombstone.
                !added.contains(&id) && matches!(op, DraftOp::Delete { .. })
            })
            .map(|op| match op {
                DraftOp::Add(event) => RecordBody::CommentEvent(event),
                DraftOp::Edit { id, author, body } => edit_event(id, author, body),
                DraftOp::Resolve {
                    id,
                    resolved,
                    author,
                } => resolve_event(id, author, resolved),
                DraftOp::Delete { id, author } => delete_event(id, author),
                DraftOp::SetDisposition {
                    id,
                    disposition,
                    author,
                } => disposition_event(id, author, disposition),
            })
            .chain(description)
            .collect()
    }
}

/// A fresh comment create event for a body drafted against diff `version`, with
/// the caller-supplied `target` and `anchor`, assigned a new identity.
pub fn draft_create(
    author: Author,
    target: CommentTarget,
    version: VersionNumber,
    anchor: Option<Anchor>,
    body: String,
) -> CommentEvent {
    CommentEvent {
        id: Ulid::new(),
        author,
        authored_at: None,
        origin: None,
        synced_marker: None,
        kind: CommentEventKind::Create(CommentCreate {
            target,
            version,
            anchor,
            body,
            disposition: None,
        }),
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
    use time::OffsetDateTime;
    use ulid::Ulid;

    use wiff_diff::parse::parse;
    use wiff_diff::{Diff, LineNo, Side};

    use super::{DraftBuffer, EffectiveComment, EffectiveDescription, draft_create};
    use crate::comment::{delete_event, disposition_event, edit_event, resolve_event};
    use crate::description::local_description;
    use crate::record::{
        Author, AuthorKind, CommentEvent, CommentTarget, Description, Disposition, RecordBody, Seq,
        VersionNumber,
    };
    use crate::review::{CommentState, DescriptionState};

    /// The human reviewer whose edits, resolves, and deletes the buffer
    /// attributes.
    fn actor() -> Author {
        Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        }
    }

    /// The agent that authors drafted additions in these tests.
    fn agent() -> Author {
        Author {
            name: "opus".to_string(),
            kind: AuthorKind::Agent,
        }
    }

    /// A committed review comment with the given identity, body, and resolved
    /// state, targeting the review overall. Its folded times are fixed so a
    /// preview asserts deterministically.
    fn committed_comment(id: u128, body: &str, resolved: bool) -> CommentState {
        CommentState {
            id: Ulid(id),
            author: actor(),
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: body.to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: actor(),
            resolved,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            disposition: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            created_seq: Seq(3),
            updated_seq: Seq(3),
        }
    }

    /// The preview state of a drafted review comment by `author`: the same shape
    /// [`DraftBuffer::apply`] yields, with the placeholder sequence and epoch
    /// time a not-yet-committed draft has.
    fn drafted_state(id: u128, author: Author, body: &str) -> CommentState {
        CommentState {
            author: author.clone(),
            updated_by: author,
            created_seq: Seq(0),
            updated_seq: Seq(0),
            ..committed_comment(id, body, false)
        }
    }

    /// A drafted review-level comment by an agent with the given identity and
    /// body.
    fn drafted(id: u128, body: &str) -> CommentEvent {
        let mut event = draft_create(
            agent(),
            CommentTarget::Review,
            VersionNumber(0),
            None,
            body.to_string(),
        );
        event.id = Ulid(id);
        event
    }

    #[test]
    fn an_empty_buffer_leaves_the_committed_comments_untouched() {
        let committed = vec![committed_comment(1, "looks fine", false)];
        let buffer = DraftBuffer::new();
        wince::assert_eq!(buffer.is_empty(), true);
        wince::assert_eq!(
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
        wince::assert_eq!(
            buffer.apply(&committed),
            vec![
                EffectiveComment {
                    comment: committed_comment(1, "looks fine", false),
                    pending: false,
                },
                EffectiveComment {
                    comment: drafted_state(2, agent(), "one more thing"),
                    pending: true,
                },
            ]
        );
    }

    #[test]
    fn editing_and_resolving_a_committed_comment_marks_it_pending() {
        let committed = vec![committed_comment(1, "old body", false)];
        let mut buffer = DraftBuffer::new();
        buffer.edit(Ulid(1), actor(), "new body".to_string());
        buffer.resolve(Ulid(1), true, actor());
        let mut expected = committed_comment(1, "new body", true);
        expected.resolved_by = Some(actor());
        wince::assert_eq!(
            buffer.apply(&committed),
            vec![EffectiveComment {
                comment: expected,
                pending: true,
            }]
        );
    }

    #[test]
    fn deleting_a_committed_comment_keeps_it_shown_as_deleted_and_pending() {
        let committed = vec![
            committed_comment(1, "a", false),
            committed_comment(2, "b", false),
        ];
        let mut buffer = DraftBuffer::new();
        buffer.delete(Ulid(1), actor());
        let mut deleted = committed_comment(1, "a", false);
        deleted.deleted = true;
        deleted.deleted_by = Some(actor());
        wince::assert_eq!(
            buffer.apply(&committed),
            vec![
                EffectiveComment {
                    comment: deleted,
                    pending: true,
                },
                EffectiveComment {
                    comment: committed_comment(2, "b", false),
                    pending: false,
                },
            ]
        );
    }

    #[test]
    fn restoring_a_deleted_committed_comment_brings_it_back_unchanged() {
        let committed = vec![committed_comment(1, "a", false)];
        let mut buffer = DraftBuffer::new();
        buffer.delete(Ulid(1), actor());
        buffer.restore(Ulid(1));
        wince::assert_eq!(buffer.is_empty(), true);
        wince::assert_eq!(
            buffer.apply(&committed),
            vec![EffectiveComment {
                comment: committed_comment(1, "a", false),
                pending: false,
            }]
        );
        wince::assert_eq!(buffer.into_records(), Vec::<RecordBody>::new());
    }

    #[test]
    fn deleting_a_committed_comment_commits_only_its_tombstone() {
        let mut buffer = DraftBuffer::new();
        buffer.edit(Ulid(1), actor(), "reworded".to_string());
        buffer.delete(Ulid(1), actor());
        wince::assert_eq!(buffer.into_records(), vec![delete_event(Ulid(1), actor())]);
    }

    #[test]
    fn a_deleted_drafted_comment_stays_shown_but_leaves_no_records() {
        let mut buffer = DraftBuffer::new();
        let id = buffer.add(drafted(2, "never mind"));
        buffer.edit(id, actor(), "second thoughts".to_string());
        buffer.delete(id, actor());
        let mut deleted = drafted_state(2, agent(), "second thoughts");
        deleted.deleted = true;
        deleted.deleted_by = Some(actor());
        wince::assert_eq!(
            buffer.apply(&[]),
            vec![EffectiveComment {
                comment: deleted,
                pending: true,
            }]
        );
        wince::assert_eq!(buffer.into_records(), Vec::<RecordBody>::new());
    }

    #[test]
    fn restoring_a_deleted_drafted_comment_recovers_its_authoring() {
        let mut buffer = DraftBuffer::new();
        let id = buffer.add(drafted(2, "keep me"));
        buffer.delete(id, actor());
        buffer.restore(id);
        wince::assert_eq!(
            buffer.into_records(),
            vec![RecordBody::CommentEvent(drafted(2, "keep me"))]
        );
    }

    #[test]
    fn setting_a_verdict_shows_it_on_the_comment_and_commits_one_event() {
        let committed = vec![committed_comment(1, "needs work", false)];
        let mut buffer = DraftBuffer::new();
        buffer.set_disposition(Ulid(1), Some(Disposition::RequestChanges), actor());
        let mut expected = committed_comment(1, "needs work", false);
        expected.disposition = Some(Disposition::RequestChanges);
        wince::assert_eq!(
            buffer.apply(&committed),
            vec![EffectiveComment {
                comment: expected,
                pending: true,
            }]
        );
        wince::assert_eq!(
            buffer.into_records(),
            vec![disposition_event(
                Ulid(1),
                actor(),
                Some(Disposition::RequestChanges)
            )]
        );
    }

    #[test]
    fn a_replaced_verdict_commits_only_the_latest() {
        let mut buffer = DraftBuffer::new();
        buffer.set_disposition(Ulid(1), Some(Disposition::RequestChanges), actor());
        buffer.set_disposition(Ulid(1), Some(Disposition::Approve), actor());
        wince::assert_eq!(
            buffer.into_records(),
            vec![disposition_event(
                Ulid(1),
                actor(),
                Some(Disposition::Approve)
            )]
        );
    }

    #[test]
    fn repeated_edits_to_one_comment_commit_as_a_single_event() {
        let mut buffer = DraftBuffer::new();
        buffer.edit(Ulid(1), actor(), "first".to_string());
        buffer.edit(Ulid(1), actor(), "final".to_string());
        wince::assert_eq!(
            buffer.into_records(),
            vec![edit_event(Ulid(1), actor(), "final".to_string())]
        );
    }

    /// v0 of a four-line added file, whose whole after side reconstructs
    /// cleanly, for rebasing a drafted line comment against.
    const V0: &str = "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+alpha
+beta
+gamma
+delta
";

    /// A drafted comment on line `line` of `f.txt`'s after side, authored
    /// against `version` with the given identity.
    fn drafted_line(id: u128, line: u32, version: VersionNumber) -> CommentEvent {
        let mut event = draft_create(
            actor(),
            CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(line).unwrap(),
                end_line: LineNo::new(line).unwrap(),
            },
            version,
            None,
            "why gamma?".to_string(),
        );
        event.id = Ulid(id);
        event
    }

    #[test]
    fn rebasing_shifts_a_drafted_line_comment_to_its_new_position() {
        // A line inserted at the top slides gamma from line 3 to line 4, and the
        // draft advances to the new version.
        let mut buffer = DraftBuffer::new();
        buffer.add(drafted_line(2, 3, VersionNumber(0)));
        let old: Diff = parse(V0).unwrap();
        let new = parse(
            "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,5 @@
+zero
+alpha
+beta
+gamma
+delta
",
        )
        .unwrap();
        buffer
            .rebase(VersionNumber(1), &new, |version| {
                wince::assert_eq!(version, VersionNumber(0));
                Ok(old.clone())
            })
            .unwrap();

        wince::assert_eq!(
            buffer.into_records(),
            vec![RecordBody::CommentEvent(drafted_line(
                2,
                4,
                VersionNumber(1)
            ))]
        );
    }

    #[test]
    fn rebasing_leaves_a_review_level_draft_untouched() {
        // A review-level draft is not tied to line content, so rebasing keeps
        // both its target and its authored-against version.
        let mut buffer = DraftBuffer::new();
        buffer.add(drafted(2, "one more thing"));
        let new = parse(V0).unwrap();
        buffer
            .rebase(VersionNumber(1), &new, |_| {
                panic!("a review draft needs no old diff")
            })
            .unwrap();
        wince::assert_eq!(
            buffer.into_records(),
            vec![RecordBody::CommentEvent(drafted(2, "one more thing"))]
        );
    }

    /// A description of `title` and `body`.
    fn description(title: &str, body: &str) -> Description {
        Description {
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    /// A committed description revision by `author`.
    fn committed_description(author: Author, title: &str, body: &str) -> DescriptionState {
        DescriptionState {
            content: description(title, body),
            author,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            origin: None,
            synced_marker: None,
        }
    }

    #[test]
    fn a_buffered_description_edit_layers_over_the_committed_one_marked_pending() {
        let committed = committed_description(actor(), "Old title", "old body");
        let mut buffer = DraftBuffer::new();
        wince::assert_eq!(
            buffer.effective_description(Some(&committed)),
            Some(EffectiveDescription {
                content: description("Old title", "old body"),
                author: actor(),
                pending: false,
            })
        );

        buffer.edit_description(agent(), description("New title", "new body"));
        wince::assert_eq!(buffer.is_empty(), false);
        wince::assert_eq!(
            buffer.effective_description(Some(&committed)),
            Some(EffectiveDescription {
                content: description("New title", "new body"),
                author: agent(),
                pending: true,
            })
        );
    }

    #[test]
    fn a_review_with_no_description_and_no_edit_has_none() {
        let buffer = DraftBuffer::new();
        wince::assert_eq!(buffer.effective_description(None), None);
    }

    #[test]
    fn committing_a_description_edit_emits_a_description_record_after_the_comments() {
        let mut buffer = DraftBuffer::new();
        buffer.resolve(Ulid(1), true, actor());
        buffer.edit_description(
            agent(),
            description("Tidy the parser", "split the lexer out"),
        );
        wince::assert_eq!(
            buffer.into_records(),
            vec![
                resolve_event(Ulid(1), actor(), true),
                local_description(
                    agent(),
                    description("Tidy the parser", "split the lexer out")
                ),
            ]
        );
    }

    #[test]
    fn commit_emits_the_buffered_events_in_order() {
        let mut buffer = DraftBuffer::new();
        buffer.add(drafted(2, "new comment"));
        buffer.resolve(Ulid(1), true, actor());
        wince::assert_eq!(
            buffer.into_records(),
            vec![
                RecordBody::CommentEvent(drafted(2, "new comment")),
                resolve_event(Ulid(1), actor(), true),
            ]
        );
    }
}
