//! Re-syncing an already-bound session against its pull request.
//!
//! A session imported from a pull request drifts as the pull request gains
//! commits, comments, and verdicts. Re-syncing brings it back in step,
//! reconciling the upstream changes against what the session already holds
//! rather than rebuilding it. The distinguishing policy is what happens to
//! divergent local state: an unpushed local edit made against unchanged upstream
//! content survives to be pushed, while a genuine upstream change wins. How the
//! diff is produced is left to the caller that prepares the source, as for a
//! first import.

use anyhow::{Result, bail};
use ulid::Ulid;
use wiff_core::determinism::new_ulid;
use wiff_core::record::{Author, CommentEvent, ForgeUrl, RecordBody};
use wiff_core::review::{ReviewState, fold};
use wiff_core::session::{LockWait, SessionLog};
use wiff_core::source::DiffSource;
use wiff_core::{RefreshOutcome, refresh_session};
use wiff_diff::SectionMatchers;
use wiff_diff::parse::parse;

use crate::pull::{reconcile_comments, reconcile_description, reconcile_reviews};
use crate::types::FetchedPullRequest;

/// The result of re-syncing a bound session: how its diff and local comments
/// fared, and how much of the forge's metadata the reconcile changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResyncOutcome {
    /// How the recaptured diff rebased the local comments, or `None` when the
    /// diff was unchanged and no new version was captured.
    pub refresh: Option<RefreshOutcome>,
    /// How many distinct comments the reconcile changed.
    pub comments: usize,
    /// How many distinct reviews the reconcile changed.
    pub reviews: usize,
    /// Whether the reconcile imported a new revision of the description.
    pub description_updated: bool,
}

/// Recapture `source` into the bound session behind `log` and reconcile the
/// fetched pull request into it, attributing the rebase re-anchors to `author`.
/// Fails without touching the session unless it is bound to `fetched`'s pull
/// request, guarding against a caller pairing a session with the wrong fetch.
///
/// The recapture and the metadata reconcile hold the session lock separately
/// rather than together. A reader between them sees the new diff and its rebased
/// comments without the not-yet-mirrored upstream metadata; a rerun completes
/// that. Should the reconcile fail after the recapture has committed, a rerun
/// still recovers, though its `refresh` is then `None` because the diff already
/// matches.
pub async fn resync_pull_request(
    log: &mut SessionLog,
    source: &dyn DiffSource,
    fetched: &FetchedPullRequest,
    author: Author,
    sections: &SectionMatchers,
) -> Result<ResyncOutcome> {
    verify_binding(log, &fetched.url)?;
    let captured = source.capture().await?;
    let refresh = refresh_session(log, &captured, author, LockWait::Block, sections)?;
    let metadata = reconcile_metadata(log, fetched, sections)?;
    Ok(ResyncOutcome {
        refresh,
        comments: metadata.comments,
        reviews: metadata.reviews,
        description_updated: metadata.description_updated,
    })
}

/// Confirm the session behind `log` is bound to the pull request at `url`,
/// erroring otherwise. Reads only the immutable header, so it runs before the
/// source is captured and no session state is disturbed on a mismatch.
fn verify_binding(log: &SessionLog, url: &ForgeUrl) -> Result<()> {
    let state = ReviewState::load(log.path())?;
    match &state.session.forge {
        Some(bound) if bound == url => Ok(()),
        Some(bound) => bail!(
            "session {} is bound to {}, not {}",
            log.id(),
            bound.as_str(),
            url.as_str()
        ),
        None => bail!("session {} is not bound to a pull request", log.id()),
    }
}

struct Metadata {
    comments: usize,
    reviews: usize,
    description_updated: bool,
}

/// Reconcile the forge's `fetched` metadata against the session's current state
/// and append the resulting events under one held lock, so a reader never sees a
/// half-mirrored reconcile. Incoming inline comments anchor against the session's
/// latest diff version, whose text is read authoritatively under the lock rather
/// than assumed to be the just-captured diff, since a concurrent refresh may have
/// advanced it.
fn reconcile_metadata(
    log: &mut SessionLog,
    fetched: &FetchedPullRequest,
    sections: &SectionMatchers,
) -> Result<Metadata> {
    let (mut lock, records) = log.lock_and_sync(LockWait::Block)?;
    let state = fold(&records)?;
    let number = state
        .latest_version()
        .ok_or_else(|| anyhow::anyhow!("a bound session has no diff version to reconcile against"))?
        .number;
    let diff = parse(&log.read_diff(number)?)?;

    let description = reconcile_description(&fetched.description, state.description.as_ref());
    let comments = reconcile_comments(
        &fetched.comments,
        &state.comments,
        &diff,
        number,
        sections,
        new_ulid,
    );
    let reviews = reconcile_reviews(&fetched.reviews, &state.comments, number, new_ulid);

    let tally = Metadata {
        comments: comments_touched(&comments),
        reviews: comments_touched(&reviews),
        description_updated: description.is_some(),
    };
    for event in description.into_iter().chain(comments).chain(reviews) {
        log.append(&mut lock, event)?;
    }
    drop(lock);
    Ok(tally)
}

/// Count the distinct comments the comment `events` apply to, so an edit and a
/// resolution of one comment tally as a single change.
fn comments_touched(events: &[RecordBody]) -> usize {
    let mut ids: Vec<Ulid> = events
        .iter()
        .filter_map(|event| match event {
            RecordBody::CommentEvent(CommentEvent { id, .. }) => Some(*id),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}
