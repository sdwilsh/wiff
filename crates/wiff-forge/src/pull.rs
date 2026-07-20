//! Reconciling a forge pull request's comments, verdicts, and description into
//! the session log.
//!
//! A pull mirrors forge comment threads, review verdicts, and the pull
//! request's description locally. This module reconciles what the forge reports
//! now against what wiff has already folded from the log, producing the
//! append-only events that bring a local review into line with upstream.

use std::collections::{HashMap, HashSet};

use ulid::Ulid;
use wiff_core::comment::{
    delete_event, import_create, import_disposition, import_edit, import_resolve,
    place_forge_anchor,
};
use wiff_core::description::mirrored_description;
use wiff_core::record::{
    Anchor, Author, AuthorKind, CommentCreate, CommentTarget, Disposition, ExternalKind,
    ExternalRef, RecordBody, VersionNumber,
};
use wiff_core::{CommentState, DescriptionState};
use wiff_diff::Diff;

use crate::types::{FetchedComment, FetchedDescription, FetchedReview};

/// Reconcile the forge's `fetched` comments against the `existing` folded
/// comments, returning the events that bring the log up to date. `diff` is the
/// session's current diff, captured as version `number`, against which an inline
/// comment's anchor is placed. `new_id` mints a stable id for each freshly
/// imported comment.
pub fn reconcile_comments(
    fetched: &[FetchedComment],
    existing: &[CommentState],
    diff: &Diff,
    number: VersionNumber,
    mut new_id: impl FnMut() -> Ulid,
) -> Vec<RecordBody> {
    let by_id: HashMap<Ulid, &CommentState> = existing
        .iter()
        .map(|comment| (comment.id, comment))
        .collect();

    // The forge object each live comment mirrors, mapped to its local id. A
    // withdrawn comment is left out: once tombstoned it stays so, and its origin
    // reappearing upstream imports as a fresh comment rather than editing the
    // tombstone.
    let mut id_by_origin: HashMap<ExternalRef, Ulid> = existing
        .iter()
        .filter(|comment| !comment.deleted)
        .filter_map(|comment| origin_of_kind(comment, ExternalKind::ReviewComment))
        .collect();
    let updates: HashSet<ExternalRef> = id_by_origin.keys().cloned().collect();

    // Reconcile each distinct forge object once, in first-seen order.
    let mut order: Vec<&FetchedComment> = Vec::new();
    let mut present: HashSet<&ExternalRef> = HashSet::new();
    for comment in fetched {
        if present.insert(&comment.origin) {
            order.push(comment);
        }
    }

    let mut events = Vec::new();

    // Track an existing comment's later body edits and thread resolutions.
    for comment in order.iter().filter(|c| updates.contains(&c.origin)) {
        let id = id_by_origin[&comment.origin];
        let current = by_id[&id];
        if current.body != comment.body {
            events.push(import_edit(
                id,
                comment.author.clone(),
                comment.origin.clone(),
                comment.authored_at,
                comment.body.clone(),
            ));
        }
        let resolved = comment.resolution.is_some();
        if current.resolved != resolved {
            events.push(import_resolve(
                id,
                resolver(comment),
                comment.origin.clone(),
                comment.authored_at,
                resolved,
            ));
        }
    }

    // Import comments new this pull, a parent ahead of any reply that answers
    // it, so the reply's create finds its parent's id already in the log.
    let new_comments: Vec<&FetchedComment> = order
        .iter()
        .copied()
        .filter(|c| !updates.contains(&c.origin))
        .collect();
    for comment in parents_before_replies(&new_comments) {
        let id = *id_by_origin
            .entry(comment.origin.clone())
            .or_insert_with(&mut new_id);
        let (target, anchor) = place_target(comment, &id_by_origin, diff, number);
        events.push(import_create(
            id,
            comment.author.clone(),
            comment.origin.clone(),
            comment.authored_at,
            CommentCreate {
                target,
                version: number,
                anchor,
                body: comment.body.clone(),
                disposition: None,
            },
        ));
        if comment.resolution.is_some() {
            events.push(import_resolve(
                id,
                resolver(comment),
                comment.origin.clone(),
                comment.authored_at,
                true,
            ));
        }
    }

    // Withdraw local comments whose forge objects no longer appear upstream. A
    // forge names neither who removed one nor when, so the tombstone is dated to
    // the pull and left in an unknown hand.
    for comment in existing {
        if comment.deleted {
            continue;
        }
        let Some(origin) = &comment.origin else {
            continue;
        };
        if origin.kind != ExternalKind::ReviewComment {
            continue;
        }
        if !present.contains(origin) {
            events.push(delete_event(comment.id, unknown_author()));
        }
    }

    events
}

/// Reconcile the forge's `fetched` reviews against the `existing` folded
/// comments, returning the events that import each review's verdict. A review
/// imports as a review-level comment holding its summary, with the mapped
/// disposition; a dismissed review has none. `number` is the current diff
/// version the imported comment is authored against. `new_id` mints a stable id
/// for each review new this pull.
///
/// Unlike a comment, a review absent from `fetched` is not withdrawn: a forge
/// dismisses a review rather than deleting it, and a dismissal arrives as a
/// still-listed review whose verdict is cleared here.
pub fn reconcile_reviews(
    fetched: &[FetchedReview],
    existing: &[CommentState],
    number: VersionNumber,
    mut new_id: impl FnMut() -> Ulid,
) -> Vec<RecordBody> {
    let by_id: HashMap<Ulid, &CommentState> = existing
        .iter()
        .map(|comment| (comment.id, comment))
        .collect();
    let mut id_by_origin: HashMap<ExternalRef, Ulid> = existing
        .iter()
        .filter(|comment| !comment.deleted)
        .filter_map(|comment| origin_of_kind(comment, ExternalKind::Verdict))
        .collect();
    let updates: HashSet<ExternalRef> = id_by_origin.keys().cloned().collect();

    // Reconcile each distinct review once, in first-seen order.
    let mut order: Vec<&FetchedReview> = Vec::new();
    let mut present: HashSet<&ExternalRef> = HashSet::new();
    for review in fetched {
        if present.insert(&review.origin) {
            order.push(review);
        }
    }

    let mut events = Vec::new();
    for review in order {
        let disposition = effective_disposition(review);
        if updates.contains(&review.origin) {
            let id = id_by_origin[&review.origin];
            let current = by_id[&id];
            if current.body != review.body {
                events.push(import_edit(
                    id,
                    review.author.clone(),
                    review.origin.clone(),
                    review.authored_at,
                    review.body.clone(),
                ));
            }
            // A disposition change, including a dismissal that clears the
            // verdict, is attributed to the review's own author rather than
            // whoever acted on the forge: fold rejects a verdict set by anyone
            // but the comment's author, and a forge that lets an admin dismiss
            // another's review does not report the dismisser here.
            if current.disposition != disposition {
                events.push(import_disposition(
                    id,
                    review.author.clone(),
                    review.origin.clone(),
                    review.authored_at,
                    disposition,
                ));
            }
        } else {
            let id = *id_by_origin
                .entry(review.origin.clone())
                .or_insert_with(&mut new_id);
            events.push(import_create(
                id,
                review.author.clone(),
                review.origin.clone(),
                review.authored_at,
                CommentCreate {
                    target: CommentTarget::Review,
                    version: number,
                    anchor: None,
                    body: review.body.clone(),
                    disposition,
                },
            ));
        }
    }

    events
}

/// Reconcile the forge's `fetched` description against the `existing` folded
/// description, returning the revision to import, or `None` when upstream is
/// unchanged since the last sync.
///
/// The incoming content is compared against `existing`'s `synced_marker`, the
/// upstream content the last sync recorded, not against the current local
/// content. That distinguishes an upstream edit, which imports, from a local
/// edit made against an untouched upstream, which is preserved for the next
/// push. First contact adopts upstream: a pull creates the session it fills, so
/// `existing` is absent then and there is no local description to preserve.
pub fn reconcile_description(
    fetched: &FetchedDescription,
    existing: Option<&DescriptionState>,
) -> Option<RecordBody> {
    let marker = fetched.content.content_marker();
    let synced = existing.and_then(|state| state.synced_marker.as_deref());
    if synced == Some(marker.as_str()) {
        return None;
    }
    Some(mirrored_description(
        fetched.author.clone(),
        fetched.content.clone(),
        fetched.origin.clone(),
        fetched.authored_at,
        marker,
    ))
}

/// The forge object an `existing` comment mirrors when it is of `kind`, paired
/// with the comment's local id. Reviews and comments each reconcile only their
/// own kind of origin, so neither withdraws the other's imported comments.
fn origin_of_kind(comment: &CommentState, kind: ExternalKind) -> Option<(ExternalRef, Ulid)> {
    let origin = comment.origin.clone()?;
    (origin.kind == kind).then_some((origin, comment.id))
}

/// The verdict an imported review holds: its mapped disposition while it
/// stands, or none once it has been dismissed.
fn effective_disposition(review: &FetchedReview) -> Option<Disposition> {
    if review.dismissed {
        None
    } else {
        review.disposition
    }
}

/// Order `comments` so a parent precedes any reply that answers it, when the
/// parent is also among `comments`. A reply whose parent is elsewhere keeps its
/// place; a reference cycle, which a forge does not produce, is broken by
/// emitting the remainder in place.
fn parents_before_replies<'a>(comments: &[&'a FetchedComment]) -> Vec<&'a FetchedComment> {
    let members: HashSet<&ExternalRef> = comments.iter().map(|comment| &comment.origin).collect();
    let mut ordered: Vec<&FetchedComment> = Vec::with_capacity(comments.len());
    let mut placed: HashSet<ExternalRef> = HashSet::new();
    let mut remaining: Vec<&FetchedComment> = comments.to_vec();
    while !remaining.is_empty() {
        let mut waiting = Vec::new();
        let mut progressed = false;
        for comment in remaining {
            let waits_on_parent = comment
                .reply_to
                .as_ref()
                .is_some_and(|parent| members.contains(parent) && !placed.contains(parent));
            if waits_on_parent {
                waiting.push(comment);
            } else {
                placed.insert(comment.origin.clone());
                ordered.push(comment);
                progressed = true;
            }
        }
        if !progressed {
            ordered.extend(waiting);
            break;
        }
        remaining = waiting;
    }
    ordered
}

/// Decide the target an imported `comment` attaches to, returning any captured
/// anchor alongside it. A reply threads onto its parent's local comment; an
/// inline comment yields an anchor only while its line is still in the diff.
fn place_target(
    comment: &FetchedComment,
    id_by_origin: &HashMap<ExternalRef, Ulid>,
    diff: &Diff,
    number: VersionNumber,
) -> (CommentTarget, Option<Anchor>) {
    if let Some(parent) = &comment.reply_to
        && let Some(&id) = id_by_origin.get(parent)
    {
        return (CommentTarget::Comment { id }, None);
    }
    match &comment.anchor {
        Some(anchor) => place_forge_anchor(
            diff,
            number,
            &anchor.path,
            anchor.side,
            anchor.start_line,
            anchor.end_line,
        ),
        None => (CommentTarget::Review, None),
    }
}

/// The author a resolution is attributed to, falling back to an unknown hand
/// when the forge reports the thread resolved without naming a resolver.
fn resolver(comment: &FetchedComment) -> Author {
    comment
        .resolution
        .as_ref()
        .and_then(|resolution| resolution.by.clone())
        .unwrap_or_else(unknown_author)
}

/// A stand-in author for a forge event with no attributed actor.
fn unknown_author() -> Author {
    Author {
        name: "unknown".to_string(),
        kind: AuthorKind::Human,
    }
}
