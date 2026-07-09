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
use crate::error::{Error, Result};
use crate::hash::SidebandHash;
use crate::rebase::rebase_line_comment;
use crate::record::{CommentReanchor, Confidence, RecordBody};
use crate::review::fold;
use crate::session::{LockAttempt, SessionLog, SyncState, read_records};

/// A tally of a refresh: the version it captured and how its comments fared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshOutcome {
    /// The number of the newly captured version.
    pub version: u32,
    /// Comments that moved to unchanged code.
    pub exact: usize,
    /// Comments relocated to their captured snippet elsewhere.
    pub approximate: usize,
    /// Comments whose reviewed code could not be located.
    pub outdated: usize,
}

/// Capture `new_diff_text` as the session's next diff version and rebase its
/// comments onto it. Returns `None` when the diff is identical to the current
/// version, so nothing is captured.
pub fn refresh_session(
    log: &mut SessionLog,
    new_diff_text: &str,
) -> Result<Option<RefreshOutcome>> {
    let state = fold(&read_records(log.path())?)?;
    let latest = state.latest_version().ok_or(Error::NoDiffVersion)?;
    if latest.diff_hash == SidebandHash::of(new_diff_text.as_bytes()) {
        return Ok(None);
    }
    let number = latest.number + 1;
    let new_diff = parse(new_diff_text)?;

    let (mut lock, sync) = match log.lock()? {
        LockAttempt::Acquired { lock, sync } => (lock, sync),
        LockAttempt::Contended => return Err(Error::Locked(log.path().to_path_buf())),
    };
    if let SyncState::Diverged { .. } = sync {
        return Err(Error::Diverged(log.path().to_path_buf()));
    }

    write_diff_version(log, &mut lock, number, new_diff_text)?;

    // Comments authored against the same version share a parsed old diff.
    let mut old_diffs: HashMap<u32, Diff> = HashMap::new();
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
                let text = read_version_diff(log, comment.version)?;
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
            Confidence::Outdated => outcome.outdated += 1,
        }
        log.append(
            &mut lock,
            RecordBody::CommentReanchor(CommentReanchor {
                id: comment.id,
                version: number,
                target: rebased.target,
                confidence: rebased.confidence,
            }),
        )?;
    }
    Ok(Some(outcome))
}

/// Read the raw diff text of an existing version from the sideband.
fn read_version_diff(log: &SessionLog, number: u32) -> Result<String> {
    let path = log.sideband_dir().join(format!("v{number}.diff"));
    std::fs::read_to_string(&path).map_err(|source| Error::io(&path, source))
}
