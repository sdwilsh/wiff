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
    Anchor, Author, BodyMarker, CommentCreate, CommentEvent, CommentEventKind, CommentNumber,
    CommentRef, CommentTarget, Confidence, Description, DescriptionRecord, DiffVersionRecord,
    Disposition, ExternalRef, FORMAT_VERSION, Record, RecordBody, Seq, SessionHeader,
    VersionNumber, comment_body_marker,
};
use crate::session::read_records;

/// The current state of a review, folded from a session's records.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewState {
    /// The session header.
    pub session: SessionHeader,
    /// The captured diff versions, in capture order (`v0` first).
    pub versions: Vec<DiffVersionRecord>,
    /// The current description, once one has been set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<DescriptionState>,
    /// The comments, in the order they were first created.
    pub comments: Vec<CommentState>,
    /// Each actor's current verdict on the review, derived from their comments'
    /// dispositions. An actor without an active verdict is absent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<ActorVerdict>,
}

/// The body fingerprint and resolution a comment last synced with the forge.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncedState {
    /// Fingerprint of the last-synced body, from [`comment_body_marker`].
    pub body_marker: BodyMarker,
    /// The last-synced resolution.
    pub resolved: bool,
}

/// An actor's current verdict across the review.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActorVerdict {
    /// Whose verdict this is.
    pub author: Author,
    /// Their current verdict.
    pub disposition: Disposition,
}

/// The current description of a review, reduced from its revisions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DescriptionState {
    /// The current title and body.
    #[serde(flatten)]
    pub content: Description,
    /// Who set the current revision.
    pub author: Author,
    /// When the current revision was authored.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// The forge object the description mirrors, persisted from an earlier
    /// revision when the current one omits it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<ExternalRef>,
    /// The upstream version this description last reconciled with, persisted from
    /// an earlier revision when the current one omits it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synced_marker: Option<String>,
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

    /// Returns the comment whose review-scoped handle is `number`, or `None`
    /// when none has it.
    pub fn comment_by_number(&self, number: CommentNumber) -> Option<&CommentState> {
        self.comments
            .iter()
            .find(|comment| comment.number == Some(number))
    }

    /// Resolves a [`CommentRef`], a ULID or review-scoped number, to the
    /// comment's stable id.
    pub fn resolve_ref(&self, reference: CommentRef) -> Result<Ulid> {
        match reference {
            CommentRef::Ulid(id) => Ok(id),
            CommentRef::Number(number) => self
                .comment_by_number(number)
                .map(|comment| comment.id)
                .ok_or(Error::UnknownCommentNumber(number)),
        }
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
    /// Its current verdict, once anyone has set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<Disposition>,
    /// How confidently it was last re-anchored, once it has been.
    pub confidence: Option<Confidence>,
    /// The forge object it mirrors, once linked. `None` until a push creates the
    /// object for a local comment, or on a comment imported without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ExternalRef>,
    /// The forge state this comment last synced with, present once it mirrors a
    /// forge object. A reconcile compares the forge's current state against this
    /// rather than against the local state, so an unpushed local edit is told
    /// apart from a genuine forge-side change and preserved for push to send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced: Option<SyncedState>,
    /// This comment's review-scoped human handle, assigned by the fold from its
    /// create-order. `None` on a comment not yet committed (a TUI draft preview)
    /// or a synthesized render-only comment, neither of which has a stable
    /// position in the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<CommentNumber>,
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
            disposition: create.disposition,
            confidence: None,
            origin: event.origin.clone(),
            // An imported create fingerprints the forge body it mirrors as the
            // comment's first synced marker; a locally authored create has no
            // forge state yet.
            synced: event.origin.as_ref().map(|_| SyncedState {
                body_marker: comment_body_marker(&create.body),
                resolved: false,
            }),
            number: None,
            created_seq: seq,
            updated_seq: seq,
        }
    }

    /// Returns this comment's display handle: its review-scoped number when it
    /// has one, else its ULID. Folded state always has a number; the ULID is the
    /// fallback for a draft preview or a synthesized comment that has none.
    pub fn handle(&self) -> String {
        match self.number {
            Some(number) => number.to_string(),
            None => self.id.to_string(),
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

    /// Advance the synced marker's body to `body`, recording that the body now
    /// matches the forge. Only a comment that already mirrors a forge object is
    /// synced, so a comment without a marker is left as is rather than seeded
    /// from local state; the next reconcile then re-imports rather than dropping
    /// an unpushed edit.
    fn mark_synced_body(&mut self, body: &str) {
        if let Some(sync) = &mut self.synced {
            sync.body_marker = comment_body_marker(body);
        }
    }

    /// Advance the synced marker's resolution to `resolved`, recording that the
    /// resolution now matches the forge. Left as is when the comment has no
    /// marker, for the same reason as [`mark_synced_body`](Self::mark_synced_body).
    fn mark_synced_resolved(&mut self, resolved: bool) {
        if let Some(sync) = &mut self.synced {
            sync.resolved = resolved;
        }
    }

    /// Stamp the time and sequence of the latest record to touch this comment,
    /// and fold an imported event's `origin` forward when present, leaving a
    /// local event's `None` untouched. Attribution of a substantive change is
    /// set by the caller; a reanchor deliberately leaves `updated_by` alone.
    fn touch(&mut self, event: &CommentEvent, at: OffsetDateTime, seq: Seq) {
        self.updated_at = at;
        self.updated_seq = seq;
        if event.origin.is_some() {
            self.origin = event.origin.clone();
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
    let mut description: Option<DescriptionState> = None;
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
            RecordBody::Description(record) => {
                fold_description(&mut description, record, at);
            }
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
    let comments: Vec<CommentState> = order
        .into_iter()
        .enumerate()
        .map(|(index, id)| {
            let mut comment = comments
                .remove(&id)
                .expect("id came from an inserted comment");
            let position =
                u32::try_from(index + 1).expect("a review holds fewer than u32::MAX comments");
            comment.number = Some(CommentNumber(position));
            comment
        })
        .collect();
    let verdicts = derive_verdicts(&comments);
    Ok(ReviewState {
        session,
        versions,
        description,
        comments,
        verdicts,
    })
}

/// Reduce each actor's comments to their current verdict on the review. Each
/// comment contributes the verdict it currently holds while it is live: a
/// withdrawn comment, or one its author has resolved (handling the objection),
/// contributes nothing. Among an actor's contributing comments a request for
/// changes dominates an approval, so one unresolved blocking comment keeps the
/// actor blocking even when they have approved elsewhere. Actors appear in the
/// order their first contributing comment does; an actor with no contributing
/// verdict is absent.
///
/// This is a pure function of the folded comment states, not a replay of the
/// event log: whether a comment blocks depends only on its final verdict and
/// resolved state, never on the order surrounding events happened to arrive.
fn derive_verdicts(comments: &[CommentState]) -> Vec<ActorVerdict> {
    let mut verdicts: Vec<ActorVerdict> = Vec::new();
    for comment in comments {
        let Some(disposition) = contributing_verdict(comment) else {
            continue;
        };
        match verdicts
            .iter_mut()
            .find(|verdict| verdict.author == comment.author)
        {
            // A request for changes dominates an approval already recorded for
            // this actor; an approval never downgrades a standing objection.
            Some(verdict) => {
                if disposition == Disposition::RequestChanges {
                    verdict.disposition = Disposition::RequestChanges;
                }
            }
            None => verdicts.push(ActorVerdict {
                author: comment.author.clone(),
                disposition,
            }),
        }
    }
    verdicts
}

/// The verdict a comment contributes to its author's aggregate, or `None` when
/// it does not count: it bears no verdict, has been withdrawn, or its author has
/// resolved it, treating their own resolve as handling the point they raised.
fn contributing_verdict(comment: &CommentState) -> Option<Disposition> {
    if comment.deleted {
        return None;
    }
    if comment.resolved && comment.resolved_by.as_ref() == Some(&comment.author) {
        return None;
    }
    comment.disposition
}

/// Fold one [`DescriptionRecord`] into the running description. Content and
/// author always come from this revision; `origin` and `synced_marker` are kept
/// from an earlier revision when this one leaves them absent. That is deliberate
/// and one-way: a revision can bind a forge object or leave the binding
/// untouched, but cannot clear one an earlier revision set.
fn fold_description(
    description: &mut Option<DescriptionState>,
    record: &DescriptionRecord,
    at: OffsetDateTime,
) {
    let previous = description.take();
    let origin = record
        .origin
        .clone()
        .or_else(|| previous.as_ref().and_then(|state| state.origin.clone()));
    let synced_marker = record
        .synced_marker
        .clone()
        .or_else(|| previous.and_then(|state| state.synced_marker));
    *description = Some(DescriptionState {
        content: record.description.clone(),
        author: record.author.clone(),
        updated_at: record.authored_at.unwrap_or(at),
        origin,
        synced_marker,
    });
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
        CommentEventKind::Link {
            forge_ref,
            synced_marker,
        } => {
            let comment = require_comment(comments, event.id, seq)?;
            // A comment binds to one forge object for its whole life; a second
            // link, or a link over an imported comment that already mirrors an
            // object, conflicts with the binding in place rather than re-points
            // it, so the log is corrupt.
            if comment.origin.is_some() {
                return Err(Error::InconsistentLog(format!(
                    "record at seq {seq} links comment {} that already mirrors a forge object",
                    event.id
                )));
            }
            // Binding is bookkeeping, not a substantive change, so like a
            // reanchor it stamps the time without reattributing the comment. The
            // link's own forge_ref is authoritative and set after touch, so it
            // wins over any origin the envelope folds forward.
            comment.touch(event, event.authored_at.unwrap_or(at), seq);
            comment.origin = Some(forge_ref.clone());
            // The forge echoes the created body, which it may have normalized;
            // record that echo's fingerprint, not the local text's. Push creates
            // a fresh, unresolved thread, so the synced resolution is false; a
            // comment resolved locally before it was linked keeps that unpushed
            // resolution for push to send rather than having it reverted on the
            // next pull.
            comment.synced = Some(SyncedState {
                body_marker: synced_marker.clone(),
                resolved: false,
            });
        }
        CommentEventKind::Edit { body } => {
            let comment = require_comment(comments, event.id, seq)?;
            comment.body = body.clone();
            comment.updated_by = event.author.clone();
            comment.touch(event, event.authored_at.unwrap_or(at), seq);
            // An imported edit mirrors the forge's body; advance the synced
            // marker so a later reconcile does not re-import it. A local edit
            // leaves the marker, keeping the unpushed change for push to send.
            if event.origin.is_some() {
                comment.mark_synced_body(body);
            }
        }
        CommentEventKind::Resolve { resolved } => {
            let comment = require_comment(comments, event.id, seq)?;
            let when = event.authored_at.unwrap_or(at);
            comment.resolved = *resolved;
            comment.resolved_by = Some(event.author.clone());
            comment.resolved_at = Some(when);
            comment.updated_by = event.author.clone();
            comment.touch(event, when, seq);
            // As with an edit, an imported resolve advances the synced marker;
            // a local one leaves it for push to send.
            if event.origin.is_some() {
                comment.mark_synced_resolved(*resolved);
            }
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
        CommentEventKind::SetDisposition { disposition } => {
            let comment = require_comment(comments, event.id, seq)?;
            // A verdict is the comment author's own; no one else may set or
            // clear it, so a foreign one is a corrupt log rather than an
            // overwrite of someone's judgement.
            if event.author != comment.author {
                return Err(Error::InconsistentLog(format!(
                    "record at seq {seq} sets a verdict on comment {} authored by someone else",
                    event.id
                )));
            }
            comment.disposition = *disposition;
            comment.updated_by = event.author.clone();
            comment.touch(event, event.authored_at.unwrap_or(at), seq);
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
