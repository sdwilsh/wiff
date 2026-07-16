//! The current review state, folded from a session's log.
//!
//! A session's log is an append-only event stream; the state a reader cares
//! about is the fold of that stream. [`fold`] walks the records once and
//! produces a [`ReviewState`]: the session header, its captured diff versions,
//! and each comment reduced from its [`CommentEvent`] chain (create, edit,
//! resolve, delete, re-anchor). This is the shared read path behind
//! `wiff render` and comment listing.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::error::{Error, Result};
use crate::record::{
    Anchor, Author, CommentCreate, CommentEvent, CommentEventKind, CommentTarget, Confidence,
    DiffVersionRecord, ExternalRef, FORMAT_VERSION, Record, RecordBody, Seq, SessionHeader,
    VersionNumber,
};
use crate::session::read_records;

/// The current state of a review, folded from a session's records.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewState {
    /// The session header.
    pub session: SessionHeader,
    /// The captured diff versions, in capture order (`v0` first).
    pub versions: Vec<DiffVersionRecord>,
    /// The comments, in the order they were first created.
    pub comments: Vec<CommentState>,
}

impl ReviewState {
    /// Read a session file and fold it into its current state.
    pub fn load(path: &Path) -> Result<Self> {
        fold(&read_records(path)?)
    }

    /// The most recently captured diff version, if any.
    pub fn latest_version(&self) -> Option<&DiffVersionRecord> {
        self.versions.last()
    }
}

/// A comment and the replies threaded beneath it.
pub struct Thread<'a> {
    /// The comment the thread hangs from: one that is not itself a reply.
    pub root: &'a CommentState,
    /// The replies under the root, flattened across nesting and ordered by
    /// `created_seq`, the log position, so they read in the order they arrived.
    pub replies: Vec<&'a CommentState>,
}

/// Group `comments` into threads. Roots (comments that are not replies) keep the
/// order they appear in `comments`; each reply attaches to the root at the top
/// of its parent chain, and the replies under a root are ordered by log
/// position. A reply whose chain does not reach a root in the slice is dropped,
/// as is one caught in a reference cycle; a folded log produces neither.
pub fn threads(comments: &[CommentState]) -> Vec<Thread<'_>> {
    let by_id: HashMap<Ulid, &CommentState> = comments.iter().map(|c| (c.id, c)).collect();
    let mut threads: Vec<Thread<'_>> = Vec::new();
    let mut thread_of_root: HashMap<Ulid, usize> = HashMap::new();
    for comment in comments {
        if comment.reply_to().is_none() {
            thread_of_root.insert(comment.id, threads.len());
            threads.push(Thread {
                root: comment,
                replies: Vec::new(),
            });
        }
    }
    for comment in comments {
        if comment.reply_to().is_none() {
            continue;
        }
        if let Some(root) = root_of(comment, &by_id)
            && let Some(&thread) = thread_of_root.get(&root.id)
        {
            threads[thread].replies.push(comment);
        }
    }
    for thread in &mut threads {
        thread.replies.sort_by_key(|comment| comment.created_seq);
    }
    threads
}

/// Walk `comment`'s `reply_to` chain up to the root of its thread. The walk is
/// bounded by the number of comments so a reference cycle, which a folded log
/// never produces, terminates with `None` rather than looping forever.
fn root_of<'a>(
    comment: &'a CommentState,
    by_id: &HashMap<Ulid, &'a CommentState>,
) -> Option<&'a CommentState> {
    let mut cursor = comment;
    for _ in 0..by_id.len() {
        match cursor.reply_to() {
            None => return Some(cursor),
            Some(parent) => cursor = by_id.get(&parent)?,
        }
    }
    None
}

/// A comment reduced from its event chain to its current state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommentState {
    /// The annotation's stable identity.
    pub id: Ulid,
    /// Who authored it.
    pub author: Author,
    /// What it is currently attached to.
    pub target: CommentTarget,
    /// The diff version it currently anchors to.
    pub version: VersionNumber,
    /// The captured content for rebasing, for line-range targets.
    pub anchor: Option<Anchor>,
    /// The current body text.
    pub body: String,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When the latest event by log order changed it. This follows log order,
    /// not wall-clock time: an imported event stamps its authored time here,
    /// which can predate an earlier event's, so this is not guaranteed to only
    /// advance.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Who made the most recent substantive change: an edit, resolve, or
    /// delete. An automatic reanchor moves the comment onto a new diff version
    /// but is not attributed here, so this stays the original author until
    /// someone edits, resolves, or withdraws the comment. It can therefore name
    /// an earlier record than [`Self::updated_seq`], which advances on a
    /// reanchor too.
    pub updated_by: Author,
    /// Whether it is resolved.
    pub resolved: bool,
    /// Who last changed its resolved state, once anyone has.
    pub resolved_by: Option<Author>,
    /// When its resolved state was last changed, once anyone has.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub resolved_at: Option<OffsetDateTime>,
    /// Whether it has been withdrawn (a tombstone; retained, not removed).
    pub deleted: bool,
    /// Who withdrew it, once withdrawn.
    pub deleted_by: Option<Author>,
    /// When it was withdrawn, once withdrawn.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub deleted_at: Option<OffsetDateTime>,
    /// How confidently it was last re-anchored, once it has been.
    pub confidence: Option<Confidence>,
    /// The forge object it mirrors, once linked. Unpopulated until a later phase
    /// mirrors forge state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ExternalRef>,
    /// The upstream version last reconciled with, once synced. Unpopulated until
    /// a later phase mirrors forge state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_marker: Option<String>,
    /// The sequence number of the creating record.
    pub created_seq: Seq,
    /// The sequence number of the most recent record that touched it, a
    /// reanchor included. This always advances and orders changes; it drives
    /// change detection after a refresh.
    pub updated_seq: Seq,
}

impl CommentState {
    /// The state of a comment at creation, before any later event in its chain
    /// is folded in. `at` and `seq` are the creating record's time and sequence
    /// number.
    pub(crate) fn from_create(
        event: &CommentEvent,
        create: &CommentCreate,
        at: OffsetDateTime,
        seq: Seq,
    ) -> Self {
        Self {
            id: event.id,
            author: event.author.clone(),
            target: create.target.clone(),
            version: create.version,
            anchor: create.anchor.clone(),
            body: create.body.clone(),
            created_at: at,
            updated_at: at,
            updated_by: event.author.clone(),
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            confidence: None,
            origin: event.origin.clone(),
            synced_marker: event.synced_marker.clone(),
            created_seq: seq,
            updated_seq: seq,
        }
    }

    /// The comment this one answers when it is a reply, taken from its target. A
    /// reply has no anchor of its own; its thread is rooted at the named comment.
    pub fn reply_to(&self) -> Option<Ulid> {
        match &self.target {
            CommentTarget::Comment { id } => Some(*id),
            _ => None,
        }
    }

    /// Who most recently changed this comment, when that is worth showing apart
    /// from its author. A reanchor is automatic bookkeeping and does not count.
    /// `None` when nothing substantive changed after creation, when the author
    /// made the latest change themselves, or when that latest change is the
    /// resolve or withdrawal already named by
    /// [`resolved_by`](Self::resolved_by)/[`deleted_by`](Self::deleted_by).
    pub fn last_changed_by(&self) -> Option<&Author> {
        if self.updated_seq == self.created_seq || self.updated_by == self.author {
            return None;
        }
        let already_named = (self.deleted && self.deleted_by.as_ref() == Some(&self.updated_by))
            || (self.resolved && self.resolved_by.as_ref() == Some(&self.updated_by));
        (!already_named).then_some(&self.updated_by)
    }

    /// Stamp the time and sequence of the latest record to touch this comment,
    /// and fold an imported event's `origin`/`synced_marker` forward when
    /// present, leaving a local event's `None` untouched. Attribution of a
    /// substantive change is set by the caller; a reanchor deliberately leaves
    /// `updated_by` alone.
    fn touch(&mut self, event: &CommentEvent, at: OffsetDateTime, seq: Seq) {
        self.updated_at = at;
        self.updated_seq = seq;
        if event.origin.is_some() {
            self.origin = event.origin.clone();
        }
        if event.synced_marker.is_some() {
            self.synced_marker = event.synced_marker.clone();
        }
    }
}

/// Fold a session's records into its current [`ReviewState`].
///
/// A log whose header version does not match this build's, older or newer, is
/// refused up front rather than partially interpreted, since a mismatched
/// format may fold differently than we expect. Within a matching log we treat
/// any inconsistency as fatal: an unrecognized record type, a mutation against a
/// comment we have never seen, or a second create for an id already introduced
/// means the log is corrupt, and silently dropping or overwriting such records
/// would misrepresent the review.
pub fn fold(records: &[Record]) -> Result<ReviewState> {
    let mut session = None;
    let mut versions = Vec::new();
    let mut order: Vec<Ulid> = Vec::new();
    let mut comments: HashMap<Ulid, CommentState> = HashMap::new();

    for record in records {
        let seq = record.seq;
        let at = record.at;
        match &record.body {
            // The header is the first record; later ones (which a valid session
            // never writes) do not displace it. Its version gates whether we can
            // safely interpret the rest of the log at all.
            RecordBody::Session(header) => {
                if session.is_none() {
                    if header.version != FORMAT_VERSION {
                        return Err(Error::UnsupportedVersion {
                            found: header.version,
                            supported: FORMAT_VERSION,
                        });
                    }
                    session = Some(header.clone());
                }
            }
            RecordBody::DiffVersion(version) => versions.push(version.clone()),
            RecordBody::CommentEvent(event) => {
                fold_comment_event(&mut comments, &mut order, event, at, seq)?;
            }
            RecordBody::Unknown => {
                return Err(Error::InconsistentLog(format!(
                    "unrecognized record type at seq {seq}"
                )));
            }
        }
    }

    let session = session.ok_or(Error::MissingHeader)?;
    let comments = order
        .into_iter()
        .map(|id| {
            comments
                .remove(&id)
                .expect("id came from an inserted comment")
        })
        .collect();
    Ok(ReviewState {
        session,
        versions,
        comments,
    })
}

/// Fold one [`CommentEvent`] into the running comment map. A create introduces a
/// comment (and is fatal if its id was already introduced); every other event
/// mutates an existing one and is fatal if that comment is unknown.
fn fold_comment_event(
    comments: &mut HashMap<Ulid, CommentState>,
    order: &mut Vec<Ulid>,
    event: &CommentEvent,
    at: OffsetDateTime,
    seq: Seq,
) -> Result<()> {
    match &event.kind {
        CommentEventKind::Create(create) => {
            if comments.contains_key(&event.id) {
                return Err(Error::InconsistentLog(format!(
                    "record at seq {seq} re-creates existing comment {}",
                    event.id
                )));
            }
            // A reply must name a comment already folded, so a thread's parent
            // is always known before its replies. Validating before the insert
            // rejects a self-reply (its own id is not yet present) as an
            // unknown parent rather than a comment that references itself.
            if let CommentTarget::Comment { id: parent } = &create.target
                && !comments.contains_key(parent)
            {
                return Err(Error::InconsistentLog(format!(
                    "record at seq {seq} replies to unknown comment {parent}"
                )));
            }
            let authored = event.authored_at.unwrap_or(at);
            order.push(event.id);
            comments.insert(
                event.id,
                CommentState::from_create(event, create, authored, seq),
            );
        }
        CommentEventKind::Edit { body } => {
            let comment = require_comment(comments, event.id, seq)?;
            comment.body = body.clone();
            comment.updated_by = event.author.clone();
            comment.touch(event, event.authored_at.unwrap_or(at), seq);
        }
        CommentEventKind::Resolve { resolved } => {
            let comment = require_comment(comments, event.id, seq)?;
            let when = event.authored_at.unwrap_or(at);
            comment.resolved = *resolved;
            comment.resolved_by = Some(event.author.clone());
            comment.resolved_at = Some(when);
            comment.updated_by = event.author.clone();
            comment.touch(event, when, seq);
        }
        CommentEventKind::Delete => {
            let comment = require_comment(comments, event.id, seq)?;
            let when = event.authored_at.unwrap_or(at);
            comment.deleted = true;
            comment.deleted_by = Some(event.author.clone());
            comment.deleted_at = Some(when);
            comment.updated_by = event.author.clone();
            comment.touch(event, when, seq);
        }
        CommentEventKind::Reanchor(reanchor) => {
            // A reply has no anchor and is never reanchored. A reanchor onto a
            // `Comment` target would turn an anchored comment into a reply, so
            // reject it as a corrupt log rather than record it.
            if let CommentTarget::Comment { .. } = &reanchor.target {
                return Err(Error::InconsistentLog(format!(
                    "record at seq {seq} reanchors comment {} onto a reply target",
                    event.id
                )));
            }
            let comment = require_comment(comments, event.id, seq)?;
            comment.version = reanchor.version;
            comment.target = reanchor.target.clone();
            comment.confidence = Some(reanchor.confidence);
            comment.touch(event, event.authored_at.unwrap_or(at), seq);
        }
    }
    Ok(())
}

/// Look up the comment a mutation targets, treating a missing one as a corrupt
/// log rather than a no-op.
fn require_comment(
    comments: &mut HashMap<Ulid, CommentState>,
    id: Ulid,
    seq: Seq,
) -> Result<&mut CommentState> {
    comments.get_mut(&id).ok_or_else(|| {
        Error::InconsistentLog(format!(
            "record at seq {seq} references unknown comment {id}"
        ))
    })
}
