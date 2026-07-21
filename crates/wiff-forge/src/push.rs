//! Planning the forge writes a push makes from the current review state.
//!
//! A push publishes the local review to the forge, but the writes it makes
//! depend on forge results: a reply cannot be posted until its parent has been
//! created and bound to a forge object. Planning what to send is kept apart from
//! the effectful sending, as a pure function of the folded review state, so it
//! can be tested with no forge in the loop. It computes one step of currently
//! sendable work; the orchestration sends that step and re-plans until nothing
//! remains.

use ulid::Ulid;
use wiff_core::record::{
    Author, CommentTarget, Description, DiffVersionRecord, Disposition, ExternalRef, RevisionId,
    VersionNumber, comment_body_marker,
};
use wiff_core::review::ActorVerdict;
use wiff_core::{CommentState, ReviewState};

use crate::types::{ForgeAnchor, OutgoingComment, OutgoingReview};

/// One step of push work computed from the current review state: the forge
/// writes that are sendable now. A push executes a plan, folds the events its
/// writes produce, and re-plans until [`is_empty`](Self::is_empty), so work that
/// depends on an earlier write (a reply to a freshly created parent) is planned
/// once that write is folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPlan {
    /// The review to submit, present when there are fresh inline comments to
    /// anchor or the pusher's verdict differs from the one last pushed. It
    /// submits the pusher's verdict together with any fresh inline comments.
    pub review: Option<OutgoingReview>,
    /// Standalone comments to post one at a time: fresh review-level comments
    /// and replies whose parent is already linked.
    pub posts: Vec<OutgoingComment>,
    /// Edits to publish for linked comments whose body has diverged from the
    /// forge since the last sync.
    pub edits: Vec<PushEdit>,
    /// Resolutions to publish for linked comments whose resolved state has
    /// diverged from the forge since the last sync.
    pub resolves: Vec<PushResolve>,
    /// The description to publish, present when the local title or body has
    /// diverged from the forge since the last sync.
    pub description: Option<Description>,
}

/// A body edit to publish for an already-linked comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushEdit {
    /// The local comment this edit publishes.
    pub comment: Ulid,
    /// The forge object to edit.
    pub at: ExternalRef,
    /// The body to publish.
    pub body: String,
}

/// A resolution change to publish for an already-linked comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushResolve {
    /// The local comment this resolution publishes.
    pub comment: Ulid,
    /// The forge object to resolve or unresolve.
    pub at: ExternalRef,
    /// The resolved state to publish.
    pub resolved: bool,
}

/// A local review that cannot be published as it stands, refused before any
/// forge write rather than sent in a degraded form.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// A line comment whose diff version has no forge commit, refused rather
    /// than published without its location.
    #[error(
        "comment {comment} is on lines of a diff captured from uncommitted work, which the forge has no commit for; commit the reviewed changes, then push"
    )]
    UnanchorableLineComment {
        /// The offending comment, named by its review number, or its full id
        /// when it has not been numbered.
        comment: String,
    },
}

impl PushPlan {
    /// Whether this plan has no work, the signal for the push loop to stop.
    pub fn is_empty(&self) -> bool {
        self.review.is_none()
            && self.posts.is_empty()
            && self.edits.is_empty()
            && self.resolves.is_empty()
            && self.description.is_none()
    }
}

/// Plan the forge writes sendable now for `author`, publishing only that
/// author's own comments. Fails with [`PlanError`] when the review holds a
/// comment that cannot be published in any form.
///
/// The failure refuses the whole push, not just the offending comment: scanning
/// every comment to build a plan aborts on the first that cannot be published,
/// before the driver executes any write. A comment is unpublishable only for a
/// reason that does not change while a push runs (its diff version has no forge
/// commit, and nothing re-pulls mid-push), so the first plan pass is a preflight
/// over the whole review: once it succeeds no later re-plan can newly fail, and
/// the push never publishes part of a review and then aborts.
pub fn plan_push(state: &ReviewState, author: &Author) -> Result<PushPlan, PlanError> {
    let parent_origin = |id: &Ulid| {
        state
            .comments
            .iter()
            .find(|comment| &comment.id == id)
            .and_then(|comment| comment.origin.clone())
    };

    let mut inline = Vec::new();
    let mut posts = Vec::new();
    let mut edits = Vec::new();
    let mut resolves = Vec::new();

    for comment in &state.comments {
        // A push authenticates as a single user; it never touches another
        // author's forge object. Deleting a withdrawn comment is a later step.
        if comment.author != *author || comment.deleted {
            continue;
        }
        match &comment.origin {
            None => match &comment.target {
                CommentTarget::Comment { id } => {
                    // A reply is sendable only once its parent has a forge
                    // object; until then it waits for a later re-plan.
                    if let Some(parent) = parent_origin(id) {
                        posts.push(outgoing(comment, None, Some(parent)));
                    }
                }
                CommentTarget::Lines { .. } => {
                    // Refuse a line comment whose version has no forge commit
                    // rather than degrading it to a locationless review-level
                    // post; the reviewer commits the work and pushes again.
                    let Some(anchor) = outgoing_anchor(comment, &state.versions) else {
                        return Err(PlanError::UnanchorableLineComment {
                            comment: comment_label(comment),
                        });
                    };
                    inline.push(outgoing(comment, Some(anchor), None));
                }
                _ => posts.push(outgoing(comment, None, None)),
            },
            Some(at) => {
                let synced = comment.synced.as_ref();
                if synced.map(|marker| &marker.body_marker)
                    != Some(&comment_body_marker(&comment.body))
                {
                    edits.push(PushEdit {
                        comment: comment.id,
                        at: at.clone(),
                        body: comment.body.clone(),
                    });
                }
                if synced.map(|marker| marker.resolved) != Some(comment.resolved) {
                    resolves.push(PushResolve {
                        comment: comment.id,
                        at: at.clone(),
                        resolved: comment.resolved,
                    });
                }
            }
        }
    }

    let my_verdict = author_disposition(&state.verdicts, author);
    // A verdict that has changed since it was last submitted must go up even
    // with no fresh inline comments to batch; an unchanged one already sent by
    // an earlier push does not resubmit. Submitting a verdict is one-way: a
    // verdict cleared or withdrawn after being pushed is not retracted from the
    // forge, since a review submission can only add a verdict, never rescind
    // one. The `is_some` guard holds that line until a forge that can express
    // retraction needs it.
    let verdict_unsent =
        my_verdict.is_some() && my_verdict != author_disposition(&state.pushed_verdicts, author);
    let review = (!inline.is_empty() || verdict_unsent).then(|| OutgoingReview {
        disposition: my_verdict,
        body: String::new(),
        comments: inline,
    });

    Ok(PushPlan {
        review,
        posts,
        edits,
        resolves,
        description: unsent_description(state),
    })
}

/// A label naming `comment` in an error a reviewer reads: its review number
/// when it has one, falling back to its full id before it has been numbered.
fn comment_label(comment: &CommentState) -> String {
    comment
        .number
        .map(|number| number.to_string())
        .unwrap_or_else(|| comment.id.to_string())
}

/// The disposition `author` holds among `verdicts`, or `None` when they hold
/// none.
fn author_disposition(verdicts: &[ActorVerdict], author: &Author) -> Option<Disposition> {
    verdicts
        .iter()
        .find(|verdict| &verdict.author == author)
        .map(|verdict| verdict.disposition)
}

/// Returns the outgoing form of `comment` placed per `anchor` and `reply_to`.
fn outgoing(
    comment: &CommentState,
    anchor: Option<ForgeAnchor>,
    reply_to: Option<ExternalRef>,
) -> OutgoingComment {
    OutgoingComment {
        comment: comment.id,
        body: comment.body.clone(),
        disposition: comment.disposition,
        anchor,
        reply_to,
    }
}

/// The inline anchor for `comment`, or `None` when it is not a line-range
/// comment or the version it anchors to has no head commit to place it against.
fn outgoing_anchor(comment: &CommentState, versions: &[DiffVersionRecord]) -> Option<ForgeAnchor> {
    let CommentTarget::Lines {
        file,
        side,
        start_line,
        end_line,
    } = &comment.target
    else {
        return None;
    };
    let commit = head_commit(versions, comment.version)?;
    Some(ForgeAnchor {
        path: file.clone(),
        side: *side,
        start_line: *start_line,
        end_line: *end_line,
        commit,
    })
}

/// The head commit the diff version `number` was captured at, when it has one.
fn head_commit(versions: &[DiffVersionRecord], number: VersionNumber) -> Option<RevisionId> {
    versions
        .iter()
        .find(|version| version.number == number)
        .and_then(|version| version.head_revision.clone())
}

/// The local description to publish when it has diverged from the forge since
/// the last sync, or `None` when it matches the synced marker or no description
/// has been set.
fn unsent_description(state: &ReviewState) -> Option<Description> {
    let description = state.description.as_ref()?;
    let matches_forge =
        description.synced_marker.as_deref() == Some(description.content.content_marker().as_str());
    (!matches_forge).then(|| description.content.clone())
}
