//! Authoring comments against a session.
//!
//! A [`DraftComment`] names who is commenting, what they are commenting on, and
//! the body. Appending it captures the diff version the comment is authored
//! against and, for a line-range target, an [`Anchor`]: the exact text of the
//! anchored lines plus a window of surrounding context, reconstructed from that
//! version's diff so the comment can later be rebased onto newer versions.
//!
//! Anchoring is best-effort. A reviewer may point at a line range purely to
//! direct the reader's attention, and those lines can sit outside the captured
//! context window. When the range cannot be reconstructed the comment is still
//! recorded, just without an anchor and so without rebasing support.

use ulid::Ulid;
use wiff_diff::parse::parse;
use wiff_diff::reconstitute::known_lines;
use wiff_diff::{LineNo, Side};

use crate::error::{Error, Result};
use crate::record::{
    Anchor, Author, CommentDelete, CommentRecord, CommentResolve, CommentTarget, DiffVersionRecord,
    Record, RecordBody,
};
use crate::review::{CommentState, fold};
use crate::session::{SessionLog, read_records};

/// The number of surrounding context lines captured on each side of a
/// line-range anchor, giving the rebaser something to match against when the
/// anchored lines themselves have moved.
const ANCHOR_CONTEXT: usize = 3;

/// A comment about to be appended to a session.
pub struct DraftComment {
    /// Who is commenting.
    pub author: Author,
    /// What the comment is attached to.
    pub target: CommentTarget,
    /// The comment body.
    pub body: String,
}

/// The outcome of appending a [`DraftComment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedComment {
    /// The new comment's stable identity.
    pub id: Ulid,
    /// The sequence number of the appended record.
    pub seq: u64,
    /// The diff version the comment was authored against.
    pub version: u32,
    /// The captured anchor, for a line-range target.
    pub anchor: Option<Anchor>,
}

impl DraftComment {
    /// Append this comment to `log`, anchoring a line-range target against the
    /// session's most recent diff version.
    pub fn append(self, log: &mut SessionLog) -> Result<AddedComment> {
        let records = read_records(log.path())?;
        let version = latest_diff_version(&records)?.number;
        let anchor = match &self.target {
            CommentTarget::Lines {
                file,
                side,
                start_line,
                end_line,
            } => capture_anchor(log, version, file, *side, *start_line, *end_line)?,
            CommentTarget::File { .. } | CommentTarget::Review => None,
        };
        let id = Ulid::new();
        let record = CommentRecord {
            id,
            author: self.author,
            target: self.target,
            version,
            anchor: anchor.clone(),
            body: self.body,
        };
        let seq = log.append_locked(RecordBody::Comment(record))?;
        Ok(AddedComment {
            id,
            seq,
            version,
            anchor,
        })
    }
}

/// Set the resolved state of an existing comment, appending a resolve record
/// attributed to `author`. The comment must already exist in the session.
pub fn set_resolved(
    log: &mut SessionLog,
    id: Ulid,
    resolved: bool,
    author: Author,
) -> Result<CommentState> {
    let comment = require_comment_in_log(log, id)?;
    log.append_locked(RecordBody::CommentResolve(CommentResolve {
        id,
        resolved,
        author,
    }))?;
    Ok(comment)
}

/// Withdraw an existing comment, appending a delete tombstone attributed to
/// `author`. The comment must already exist in the session.
pub fn delete_comment(log: &mut SessionLog, id: Ulid, author: Author) -> Result<CommentState> {
    let comment = require_comment_in_log(log, id)?;
    log.append_locked(RecordBody::CommentDelete(CommentDelete { id, author }))?;
    Ok(comment)
}

/// Fold the session and return the current state of comment `id`, or
/// [`Error::UnknownComment`] when the session has no such comment.
fn require_comment_in_log(log: &SessionLog, id: Ulid) -> Result<CommentState> {
    let state = fold(&read_records(log.path())?)?;
    state
        .comments
        .into_iter()
        .find(|comment| comment.id == id)
        .ok_or(Error::UnknownComment(id))
}

/// The most recently captured diff version among `records`.
fn latest_diff_version(records: &[Record]) -> Result<&DiffVersionRecord> {
    records
        .iter()
        .rev()
        .find_map(|record| match &record.body {
            RecordBody::DiffVersion(version) => Some(version),
            _ => None,
        })
        .ok_or(Error::NoDiffVersion)
}

/// Capture the anchor for a line range on one side of `file` in diff version
/// `number`: the exact text of lines `start..=end`, plus up to
/// [`ANCHOR_CONTEXT`] known lines of context on each side.
///
/// The diff is captured with wide context, so its reconstructed lines cover most
/// or all of each changed file and the range is almost always present. When it
/// is not, because the target sits beyond the captured window, this returns
/// `Ok(None)`: the comment stands as a locator without a rebasable anchor. A
/// file absent from the diff entirely is a hard error, since there is nothing
/// for the comment to point into.
fn capture_anchor(
    log: &SessionLog,
    number: u32,
    file: &str,
    side: Side,
    start: LineNo,
    end: LineNo,
) -> Result<Option<Anchor>> {
    let path = log.sideband_dir().join(format!("v{number}.diff"));
    let text = std::fs::read_to_string(&path).map_err(|source| Error::io(&path, source))?;
    let diff = parse(&text)?;
    let file_diff = diff
        .files
        .iter()
        .find(|candidate| candidate.display_path() == file)
        .ok_or_else(|| Error::Anchor(format!("{file} is not part of diff v{number}")))?;

    let lines = known_lines(file_diff, side);
    let Some(first) = lines
        .iter()
        .position(|(lineno, _)| *lineno >= start)
        .filter(|&index| lines[index].0 == start)
    else {
        return Ok(None);
    };
    let want = (end.get() - start.get() + 1) as usize;
    let snippet: Vec<String> = lines
        .iter()
        .skip(first)
        .take(want)
        .map(|(_, text)| text.clone())
        .collect();
    // The requested lines must be present as one contiguous run: a gap in the
    // range means the missing lines fall outside the captured window, so there
    // is nothing to anchor exactly to and the comment stays a bare locator.
    let last_lineno = lines[first + snippet.len() - 1].0;
    if snippet.len() != want || last_lineno != end {
        return Ok(None);
    }

    let context_before = lines[first.saturating_sub(ANCHOR_CONTEXT)..first]
        .iter()
        .map(|(_, text)| text.clone())
        .collect();
    let after_start = first + snippet.len();
    let after_end = (after_start + ANCHOR_CONTEXT).min(lines.len());
    let context_after = lines[after_start..after_end]
        .iter()
        .map(|(_, text)| text.clone())
        .collect();

    Ok(Some(Anchor {
        snippet,
        context_before,
        context_after,
    }))
}
