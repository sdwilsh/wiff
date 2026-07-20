//! Reconciling a forge pull request's comments into the session log.
//!
//! A pull mirrors forge comment threads locally. This module reconciles what
//! the forge reports now against what wiff has already folded from the log,
//! producing the append-only events that bring a local review into line with
//! upstream.

use std::collections::{HashMap, HashSet};

use ulid::Ulid;
use wiff_core::CommentState;
use wiff_core::comment::{
    delete_event, import_create, import_edit, import_resolve, place_forge_anchor,
};
use wiff_core::record::{
    Anchor, Author, AuthorKind, CommentCreate, CommentTarget, ExternalRef, RecordBody,
    VersionNumber,
};
use wiff_diff::Diff;

use crate::types::FetchedComment;

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
        .filter_map(|comment| comment.origin.clone().map(|origin| (origin, comment.id)))
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
        if !present.contains(origin) {
            events.push(delete_event(comment.id, unknown_author()));
        }
    }

    events
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
