//! Authoring comments against a review session: attributing each to an author,
//! tying it to the diff version under review, and, for a line-range target,
//! anchoring it so it can be relocated onto later versions when the reviewed
//! lines move. Anchoring is best-effort; a range outside the captured diff
//! leaves the comment a bare locator without rebasing support.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_diff::parse::parse;
use wiff_diff::reconstitute::known_lines;
use wiff_diff::{Diff, FileDiff, LineNo, Side};

use crate::error::{Error, Result};
use crate::record::{
    Anchor, Author, CommentCreate, CommentEvent, CommentEventKind, CommentReanchor, CommentTarget,
    DiffVersionRecord, Disposition, ExternalRef, Record, RecordBody, Seq, VersionNumber,
};
use crate::review::{CommentState, fold};
use crate::session::{LockWait, SessionLog};

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
    /// The verdict the comment holds from creation, when it has one.
    pub disposition: Option<Disposition>,
}

/// The outcome of appending a [`DraftComment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedComment {
    /// The new comment's stable identity.
    pub id: Ulid,
    /// The sequence number of the appended record.
    pub seq: Seq,
    /// The diff version the comment was authored against.
    pub version: VersionNumber,
    /// The captured anchor, for a line-range target.
    pub anchor: Option<Anchor>,
}

impl DraftComment {
    /// Append this comment to `log`, anchoring a line-range target against the
    /// session's most recent diff version. The version and anchor are read under
    /// the same lock that appends the comment.
    pub fn append(self, log: &mut SessionLog, wait: LockWait) -> Result<AddedComment> {
        let (mut lock, records) = log.lock_and_sync(wait)?;
        let version = latest_diff_version(&records)?.number;
        let anchor = match &self.target {
            CommentTarget::Lines { .. } => capture_target_anchor(log, version, &self.target)?,
            // A reply names the comment it answers. Confirm that comment exists
            // before writing: fold rejects a reply to an unknown id as a corrupt
            // log, so appending an unchecked reply would make the whole session
            // unreadable for every later reader. Refuse a reply to a comment
            // already withdrawn locally; fold still tolerates a reply whose
            // parent was withdrawn on another actor's copy after the reply.
            CommentTarget::Comment { id } => {
                let parent = require_comment_in_records(&records, *id)?;
                if parent.deleted {
                    return Err(Error::WithdrawnComment(*id));
                }
                None
            }
            CommentTarget::File { .. } | CommentTarget::Review => None,
        };
        let id = Ulid::new();
        let event = CommentEvent {
            id,
            author: self.author,
            authored_at: None,
            origin: None,
            kind: CommentEventKind::Create(CommentCreate {
                target: self.target,
                version,
                anchor: anchor.clone(),
                body: self.body,
                disposition: self.disposition,
            }),
        };
        let seq = log.append(&mut lock, RecordBody::CommentEvent(event))?;
        Ok(AddedComment {
            id,
            seq,
            version,
            anchor,
        })
    }
}

/// The comments a batch anchor-capture could not anchor and why, for the caller
/// to report to the reviewer.
#[derive(Debug, Default)]
pub struct AnchorFailures {
    /// The number of line-range drafts left without an anchor by an unreadable or
    /// unparsable version diff, or a file absent from it.
    pub unanchored: usize,
    /// The distinct causes behind those comments, deduplicated by message and in
    /// first-encounter order: several comments sharing one fault produce one
    /// message, not a flood of identical ones.
    pub errors: Vec<Error>,
}

impl AnchorFailures {
    /// Returns a one-line summary of how many comments could not be anchored, or
    /// `None` when every comment anchored. Suitable for a status line that has no
    /// room for the individual causes.
    pub fn summary(&self) -> Option<String> {
        (self.unanchored > 0).then(|| {
            format!(
                "{} comment{} could not be anchored",
                self.unanchored,
                if self.unanchored == 1 { "" } else { "s" }
            )
        })
    }
}

/// Fill the anchor for every line-range draft among `drafts` that lacks one,
/// reading the diff of the version each was authored against. Returns the
/// comments left unanchored by a genuine failure and the distinct faults behind
/// them; those comments still commit, as bare locators.
///
/// A comment drafted in the TUI is buffered without an anchor; filling it before
/// commit gives it the same rebasing support as one authored through the CLI.
/// A drafted comment's target line numbers are in its own `version`'s coordinate
/// space, since [`DraftBuffer::rebase`] moves the two together. A written
/// version's diff is fixed once its indexing record is appended, so this reads it
/// without the session lock and cannot observe a partial or superseded diff.
///
/// A range outside the captured window is a deliberate bare locator, not counted
/// as a failure; a file absent from the version's diff is counted and reported
/// but still commits, so one bad draft does not cost the reviewer the whole batch.
///
/// [`DraftBuffer::rebase`]: crate::draft::DraftBuffer::rebase
pub fn capture_draft_anchors(log: &SessionLog, drafts: &mut [RecordBody]) -> AnchorFailures {
    // Parse each version's diff once and reuse it for every draft against that
    // version: a batch usually comments several times on one version.
    let mut diffs: HashMap<VersionNumber, Option<Diff>> = HashMap::new();
    // Several comments can share one fault; report its message once while still
    // counting each comment it left unanchored.
    let mut reported: HashSet<String> = HashSet::new();
    let mut failures = AnchorFailures::default();
    for draft in drafts {
        let RecordBody::CommentEvent(CommentEvent {
            kind: CommentEventKind::Create(create),
            ..
        }) = draft
        else {
            continue;
        };
        if create.anchor.is_some() || !matches!(create.target, CommentTarget::Lines { .. }) {
            continue;
        }
        let diff = match diffs.entry(create.version) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let parsed = match read_version_diff(log, create.version) {
                    Ok(diff) => Some(diff),
                    Err(err) => {
                        if reported.insert(err.to_string()) {
                            failures.errors.push(err);
                        }
                        None
                    }
                };
                entry.insert(parsed)
            }
        };
        let Some(diff) = diff else {
            failures.unanchored += 1;
            continue;
        };
        match anchor_in_diff(diff, create.version, &create.target) {
            Ok(anchor) => create.anchor = anchor,
            Err(err) => {
                failures.unanchored += 1;
                if reported.insert(err.to_string()) {
                    failures.errors.push(err);
                }
            }
        }
    }
    failures
}

/// Capture the anchor for a line-range `target` in diff version `number` by
/// reading and parsing that version's sideband diff. Yields `Ok(None)` for a
/// range beyond the captured window, and an error for a file absent from the
/// diff.
fn capture_target_anchor(
    log: &SessionLog,
    number: VersionNumber,
    target: &CommentTarget,
) -> Result<Option<Anchor>> {
    let diff = read_version_diff(log, number)?;
    anchor_in_diff(&diff, number, target)
}

fn read_version_diff(log: &SessionLog, number: VersionNumber) -> Result<Diff> {
    Ok(parse(&log.read_diff(number)?)?)
}

/// Returns the anchor for a line-range `target` within `diff`, using `number`
/// only for the error message. Yields `Ok(None)` for a non-line target or a range
/// beyond the captured window, and an error for a file absent from the diff.
pub(crate) fn anchor_in_diff(
    diff: &Diff,
    number: VersionNumber,
    target: &CommentTarget,
) -> Result<Option<Anchor>> {
    let CommentTarget::Lines {
        file,
        side,
        start_line,
        end_line,
    } = target
    else {
        return Ok(None);
    };
    let file_diff = diff
        .files
        .iter()
        .find(|candidate| candidate.display_path() == file)
        .ok_or_else(|| Error::Anchor(format!("{file} is not part of diff v{number}")))?;
    Ok(anchor_in_file_diff(
        file_diff,
        *side,
        *start_line,
        *end_line,
    ))
}

/// Place a forge anchor onto wiff's comment model against `diff`, captured as
/// version `number`. Falls back to a bare `File` target on the same path when
/// the anchored line no longer matches its side of the diff.
pub fn place_forge_anchor(
    diff: &Diff,
    number: VersionNumber,
    path: &str,
    side: Side,
    start_line: LineNo,
    end_line: LineNo,
) -> (CommentTarget, Option<Anchor>) {
    let lines = CommentTarget::Lines {
        file: path.to_string(),
        side,
        start_line,
        end_line,
    };
    match anchor_in_diff(diff, number, &lines) {
        Ok(Some(anchor)) => (lines, Some(anchor)),
        _ => (
            CommentTarget::File {
                file: path.to_string(),
            },
            None,
        ),
    }
}

/// Set the resolved state of an existing comment, appending a resolve record
/// attributed to `author`. The comment must already exist in the session.
pub fn set_resolved(
    log: &mut SessionLog,
    id: Ulid,
    resolved: bool,
    author: Author,
    wait: LockWait,
) -> Result<CommentState> {
    let (mut lock, records) = log.lock_and_sync(wait)?;
    let comment = require_comment_in_records(&records, id)?;
    log.append(&mut lock, resolve_event(id, author, resolved))?;
    Ok(comment)
}

/// Withdraw an existing comment, appending a delete tombstone attributed to
/// `author`. The comment must already exist in the session.
pub fn delete_comment(
    log: &mut SessionLog,
    id: Ulid,
    author: Author,
    wait: LockWait,
) -> Result<CommentState> {
    let (mut lock, records) = log.lock_and_sync(wait)?;
    let comment = require_comment_in_records(&records, id)?;
    log.append(&mut lock, delete_event(id, author))?;
    Ok(comment)
}

/// Set (or clear) `author`'s verdict on their comment `id`, appending
/// a disposition event. Only the comment's author may set it: a mismatch is
/// refused here rather than written, since fold rejects a foreign verdict as a
/// corrupt log.
pub fn set_disposition(
    log: &mut SessionLog,
    id: Ulid,
    disposition: Option<Disposition>,
    author: Author,
    wait: LockWait,
) -> Result<CommentState> {
    let (mut lock, records) = log.lock_and_sync(wait)?;
    let comment = require_comment_in_records(&records, id)?;
    if comment.author != author {
        return Err(Error::ForeignDisposition(id));
    }
    log.append(&mut lock, disposition_event(id, author, disposition))?;
    Ok(comment)
}

/// Build an edit event for `id` by `author`.
pub fn edit_event(id: Ulid, author: Author, body: String) -> RecordBody {
    comment_event(id, author, CommentEventKind::Edit { body })
}

/// Build a resolve event for `id` by `author`.
pub fn resolve_event(id: Ulid, author: Author, resolved: bool) -> RecordBody {
    comment_event(id, author, CommentEventKind::Resolve { resolved })
}

/// Build a delete tombstone for `id` by `author`.
pub fn delete_event(id: Ulid, author: Author) -> RecordBody {
    comment_event(id, author, CommentEventKind::Delete)
}

/// Build a set-verdict event for `id` by `author`, or a clear when `disposition`
/// is `None`.
pub fn disposition_event(id: Ulid, author: Author, disposition: Option<Disposition>) -> RecordBody {
    comment_event(id, author, CommentEventKind::SetDisposition { disposition })
}

/// Build a re-anchor event for `id` by `author`, recording the new target and
/// confidence.
pub fn reanchor_event(id: Ulid, author: Author, reanchor: CommentReanchor) -> RecordBody {
    comment_event(id, author, CommentEventKind::Reanchor(reanchor))
}

/// Build a link event binding comment `id` to `forge_ref`, the forge object a
/// push created for it, by `author`.
pub fn link_event(id: Ulid, author: Author, forge_ref: ExternalRef) -> RecordBody {
    comment_event(id, author, CommentEventKind::Link { forge_ref })
}

/// Build an imported create for a new comment `id` mirroring `origin`, the forge
/// object it was read from, by `author` at the forge's own `authored_at`, with
/// `create` its initial state.
pub fn import_create(
    id: Ulid,
    author: Author,
    origin: ExternalRef,
    authored_at: OffsetDateTime,
    create: CommentCreate,
) -> RecordBody {
    import_event(
        id,
        author,
        origin,
        authored_at,
        CommentEventKind::Create(create),
    )
}

/// Build an imported edit of comment `id` mirroring `origin` by `author` at the
/// forge's own `authored_at`, revising its body to the upstream text.
pub fn import_edit(
    id: Ulid,
    author: Author,
    origin: ExternalRef,
    authored_at: OffsetDateTime,
    body: String,
) -> RecordBody {
    import_event(
        id,
        author,
        origin,
        authored_at,
        CommentEventKind::Edit { body },
    )
}

/// Build an imported resolve of comment `id` mirroring `origin` by `author` at
/// the forge's own `authored_at`, matching the upstream thread's resolution.
pub fn import_resolve(
    id: Ulid,
    author: Author,
    origin: ExternalRef,
    authored_at: OffsetDateTime,
    resolved: bool,
) -> RecordBody {
    import_event(
        id,
        author,
        origin,
        authored_at,
        CommentEventKind::Resolve { resolved },
    )
}

/// Build an imported delete of comment `id` mirroring `origin` by `author` at
/// the forge's own `authored_at`.
pub fn import_delete(
    id: Ulid,
    author: Author,
    origin: ExternalRef,
    authored_at: OffsetDateTime,
) -> RecordBody {
    import_event(id, author, origin, authored_at, CommentEventKind::Delete)
}

/// Build an imported verdict change for comment `id` mirroring `origin` by
/// `author` at the forge's own `authored_at`, setting the disposition or
/// clearing it when `disposition` is `None`.
pub fn import_disposition(
    id: Ulid,
    author: Author,
    origin: ExternalRef,
    authored_at: OffsetDateTime,
    disposition: Option<Disposition>,
) -> RecordBody {
    import_event(
        id,
        author,
        origin,
        authored_at,
        CommentEventKind::SetDisposition { disposition },
    )
}

/// Wrap a locally-authored event kind in a [`CommentEvent`] envelope: no forge
/// origin, and no authored-at (its time is the record's).
fn comment_event(id: Ulid, author: Author, kind: CommentEventKind) -> RecordBody {
    RecordBody::CommentEvent(CommentEvent {
        id,
        author,
        authored_at: None,
        origin: None,
        kind,
    })
}

/// Wrap an imported event kind in a [`CommentEvent`] envelope, naming the forge
/// object it mirrors and the forge's own authored time.
fn import_event(
    id: Ulid,
    author: Author,
    origin: ExternalRef,
    authored_at: OffsetDateTime,
    kind: CommentEventKind,
) -> RecordBody {
    RecordBody::CommentEvent(CommentEvent {
        id,
        author,
        authored_at: Some(authored_at),
        origin: Some(origin),
        kind,
    })
}

/// Fold `records` and return the current state of comment `id`, or
/// [`Error::UnknownComment`] when they hold no such comment.
fn require_comment_in_records(records: &[Record], id: Ulid) -> Result<CommentState> {
    let state = fold(records)?;
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

/// Returns the anchor for lines `start..=end` on `side` within a single file's
/// diff: the exact text of those lines, plus up to [`ANCHOR_CONTEXT`] known
/// context lines on each side. Yields `None` when the range is not present as
/// one contiguous run in the captured window.
fn anchor_in_file_diff(
    file_diff: &FileDiff,
    side: Side,
    start: LineNo,
    end: LineNo,
) -> Option<Anchor> {
    // The line count below is `end - start + 1` on `NonZeroU32` and underflows
    // for a reversed range. Callers construct ranges low-to-high, so guarding
    // here is defensive.
    if end < start {
        return None;
    }
    let lines = known_lines(file_diff, side);
    // `position` guarantees `lines[first]` exists, so `snippet` below holds at
    // least one line and indexing it by `snippet.len() - 1` cannot underflow.
    let first = lines
        .iter()
        .position(|(lineno, _)| *lineno >= start)
        .filter(|&index| lines[index].0 == start)?;
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
        return None;
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

    Some(Anchor {
        snippet,
        context_before,
        context_after,
    })
}
