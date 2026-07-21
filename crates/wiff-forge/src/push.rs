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
    Author, CommentTarget, Description, DiffVersionRecord, ExternalRef, RevisionId, VersionNumber,
    comment_body_marker,
};
use wiff_core::{CommentState, ReviewState};

use crate::types::{ForgeAnchor, OutgoingComment, OutgoingReview};

/// One step of push work computed from the current review state: the forge
/// writes that are sendable now. A push executes a plan, folds the events its
/// writes produce, and re-plans until [`is_empty`](Self::is_empty), so work that
/// depends on an earlier write (a reply to a freshly created parent) is planned
/// once that write is folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPlan {
    /// The batched review to submit, present when there are fresh inline
    /// comments to anchor; the pusher's verdict is submitted with it.
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
/// author's own comments.
pub fn plan_push(state: &ReviewState, author: &Author) -> PushPlan {
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
                _ => match outgoing_anchor(comment, &state.versions) {
                    Some(anchor) => inline.push(outgoing(comment, Some(anchor), None)),
                    None => posts.push(outgoing(comment, None, None)),
                },
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

    let review = (!inline.is_empty()).then(|| OutgoingReview {
        disposition: state
            .verdicts
            .iter()
            .find(|verdict| &verdict.author == author)
            .map(|verdict| verdict.disposition),
        body: String::new(),
        comments: inline,
    });

    PushPlan {
        review,
        posts,
        edits,
        resolves,
        description: unsent_description(state),
    }
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
/// comment or the version it anchors to has no head commit to place it against,
/// in which case it posts at review level instead.
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
