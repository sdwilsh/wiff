//! Capturing a new diff version into a session and rebasing its comments.
//!
//! A refresh appends the freshly captured diff as the next version, then moves
//! every live line-range comment forward onto it via the [`rebase`](crate::rebase)
//! engine, recording each move as an append-only re-anchor event. The whole
//! sequence is written under one held lock so the new version and its rebased
//! comments land together.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use wiff_diff::Diff;
use wiff_diff::parse::parse;

use crate::capture::write_diff_version;
use crate::comment::reanchor_event;
use crate::error::{Error, Result};
use crate::hash::SidebandHash;
use crate::rebase::rebase_line_comment;
use crate::record::{Author, CommentReanchor, Confidence, VersionNumber};
use crate::review::fold;
use crate::session::{LockWait, SessionLog};
use crate::source::CapturedDiff;

/// A tally of a refresh: the version it captured and how its comments fared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshOutcome {
    /// The number of the newly captured version.
    pub version: VersionNumber,
    /// Comments that moved to unchanged code.
    pub exact: usize,
    /// Comments relocated to their captured snippet elsewhere.
    pub approximate: usize,
    /// Comments repositioned by tracing the shared base when their reviewed
    /// content itself was gone.
    pub relocated: usize,
    /// Comments whose reviewed code could not be located.
    pub outdated: usize,
}

/// Capture `captured` as the session's next diff version and rebase its comments
/// onto it. Returns `None` when the diff is identical to the current version, so
/// nothing is captured.
pub fn refresh_session(
    log: &mut SessionLog,
    captured: &CapturedDiff,
    author: Author,
    wait: LockWait,
) -> Result<Option<RefreshOutcome>> {
    let (mut lock, records) = log.lock_and_sync(wait)?;
    let state = fold(&records)?;
    let latest = state.latest_version().ok_or(Error::NoDiffVersion)?;
    if latest.diff_hash == SidebandHash::of(captured.text.as_bytes()) {
        return Ok(None);
    }
    let number = latest.number.next();
    let new_diff = parse(&captured.text)?;

    write_diff_version(
        log,
        &mut lock,
        number,
        &captured.text,
        captured.base_revision.clone(),
        captured.head_revision.clone(),
    )?;

    // Comments authored against the same version share a parsed old diff.
    let mut old_diffs: HashMap<VersionNumber, Diff> = HashMap::new();
    let mut outcome = RefreshOutcome {
        version: number,
        ..Default::default()
    };
    for comment in &state.comments {
        if comment.deleted {
            continue;
        }
        let old_diff = match old_diffs.entry(comment.version) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let text = log.read_diff(comment.version)?;
                entry.insert(parse(&text)?)
            }
        };
        let Some(rebased) = rebase_line_comment(
            &comment.target,
            comment.anchor.as_ref(),
            old_diff,
            &new_diff,
        ) else {
            continue;
        };
        match rebased.confidence {
            Confidence::Exact => outcome.exact += 1,
            Confidence::Approximate => outcome.approximate += 1,
            Confidence::Relocated => outcome.relocated += 1,
            Confidence::Outdated => outcome.outdated += 1,
        }
        log.append(
            &mut lock,
            reanchor_event(
                comment.id,
                author.clone(),
                CommentReanchor {
                    version: number,
                    target: rebased.target,
                    confidence: rebased.confidence,
                },
            ),
        )?;
    }
    Ok(Some(outcome))
}
