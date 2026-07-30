//! Assembling a fetched pull request into a new review session.
//!
//! An imported review is built from two places that must agree: the forge, which
//! reports the description, comments, and verdicts; and a diff source, which
//! produces the unified diff the review anchors against. This module turns the
//! forge's metadata into the events a fresh session opens with and binds that
//! session to the pull request, leaving how the diff was produced to the caller
//! that prepares the source.

use anyhow::Result;
use std::path::Path;
use ulid::Ulid;
use wiff_core::SessionId;
use wiff_core::capture::create_forge_session;
use wiff_core::identity::ProjectIdentity;
use wiff_core::record::{CommentEventKind, ExternalKind, RecordBody, VersionNumber};
use wiff_core::source::DiffSource;
use wiff_diff::Diff;
use wiff_diff::parse::parse;

use crate::pull::{reconcile_comments, reconcile_description, reconcile_reviews};
use crate::types::FetchedPullRequest;

/// The result of importing a pull request: the bound session, the version
/// captured, and how much of the forge's metadata the session now mirrors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    /// The imported session's id.
    pub session: SessionId,
    /// The diff version the import captured, always the first (`v0`).
    pub version: VersionNumber,
    /// How many of the pull request's comments the session mirrors: inline
    /// comments, their replies, and review-level comments alike.
    pub comments: usize,
    /// How many of the pull request's reviews the session mirrors.
    pub reviews: usize,
    /// Whether the pull request's description was mirrored into the session.
    pub description_imported: bool,
}

/// Where an imported session is written and under which id it is bound.
pub struct ImportRequest<'a> {
    /// The id the session is created under, which the bound header records.
    pub session: SessionId,
    /// The sessions root the session file is written beneath.
    pub base: &'a Path,
    /// The project the session belongs to.
    pub identity: &'a ProjectIdentity,
    /// The working directory recorded as the session's origin.
    pub cwd: &'a Path,
}

/// Capture `source` as the first diff version and create a session bound to the
/// fetched pull request under `req.session`, opening it with the pull request's
/// mirrored description, comments, and reviews.
///
/// The caller has already fetched the pull request and prepared `source` from
/// it: an in-repo pull pins the fetched commits and hands in a `GitSource` over
/// them, a repo-less pull hands in the diff it built from the base and head
/// blobs. `req.session` is minted before the fetch because an in-repo caller
/// keys its pins on it while the session file does not yet exist, and the bound
/// header records that same id.
pub async fn import_pull_request(
    source: &dyn DiffSource,
    fetched: &FetchedPullRequest,
    req: &ImportRequest<'_>,
) -> Result<ImportOutcome> {
    let captured = source.capture().await?;
    let diff = parse(&captured.text)?;
    let (events, mirrored) = mirror_events(fetched, &diff, VersionNumber(0));

    create_forge_session(
        req.base,
        req.identity,
        req.cwd,
        fetched.url.clone(),
        req.session,
        &captured,
        events,
    )?;

    Ok(ImportOutcome {
        session: req.session,
        version: VersionNumber(0),
        comments: mirrored.comments,
        reviews: mirrored.reviews,
        description_imported: mirrored.description_imported,
    })
}

/// The tally of what an import mirrored.
struct Mirrored {
    comments: usize,
    reviews: usize,
    description_imported: bool,
}

/// Turn the forge's `fetched` metadata into the events a fresh session opens
/// with, anchoring inline comments against `diff` at version `number`, and tally
/// what those events mirror. The session is new, so each incoming object imports
/// as a fresh create rather than reconciling against anything already present.
fn mirror_events(
    fetched: &FetchedPullRequest,
    diff: &Diff,
    number: VersionNumber,
) -> (Vec<RecordBody>, Mirrored) {
    let description = reconcile_description(&fetched.description, None);
    let comments = reconcile_comments(&fetched.comments, &[], diff, number, Ulid::new);
    let reviews = reconcile_reviews(&fetched.reviews, &[], number, Ulid::new);

    let mirrored = Mirrored {
        comments: created_count(&comments, ExternalKind::ReviewComment),
        reviews: created_count(&reviews, ExternalKind::Verdict),
        description_imported: description.is_some(),
    };
    let events = description
        .into_iter()
        .chain(comments)
        .chain(reviews)
        .collect();
    (events, mirrored)
}

/// How many of `events` create a comment mirroring a forge object of `kind`.
fn created_count(events: &[RecordBody], kind: ExternalKind) -> usize {
    events
        .iter()
        .filter(|event| match event {
            RecordBody::CommentEvent(event) => {
                matches!(event.kind, CommentEventKind::Create(_))
                    && event
                        .origin
                        .as_ref()
                        .is_some_and(|origin| origin.kind == kind)
            }
            _ => false,
        })
        .count()
}
