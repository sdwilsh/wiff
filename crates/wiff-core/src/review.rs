//! The current review state, folded from a session's log.
//!
//! A session's log is an append-only event stream; the state a reader cares
//! about is the fold of that stream. [`fold`] walks the records once and
//! produces a [`ReviewState`]: the session header, its captured diff versions,
//! and each comment reduced from its event chain (create, edit, resolve,
//! delete, re-anchor). This is the shared read path behind `wiff render` and
//! comment listing.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;
use ulid::Ulid;

use crate::error::{Error, Result};
use crate::record::{
    Anchor, Author, CommentTarget, Confidence, DiffVersionRecord, FORMAT_VERSION, Record,
    RecordBody, SessionHeader,
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
    pub version: u32,
    /// The captured content for rebasing, for line-range targets.
    pub anchor: Option<Anchor>,
    /// The current body text.
    pub body: String,
    /// Whether it is resolved.
    pub resolved: bool,
    /// Whether it has been withdrawn (a tombstone; retained, not removed).
    pub deleted: bool,
    /// How confidently it was last re-anchored, once it has been.
    pub confidence: Option<Confidence>,
    /// The sequence number of the creating record.
    pub created_seq: u64,
    /// The sequence number of the most recent record that changed it.
    pub updated_seq: u64,
}

/// Fold a session's records into its current [`ReviewState`].
///
/// A log written by a newer, incompatible format is refused up front via the
/// header's version rather than partially interpreted, since we cannot know how
/// to fold records we do not understand. Within a compatible log we treat any
/// inconsistency as fatal: an unrecognized record type, or a mutation that
/// references a comment we have never seen, means the log is corrupt, and
/// silently dropping such records would misrepresent the review.
pub fn fold(records: &[Record]) -> Result<ReviewState> {
    let mut session = None;
    let mut versions = Vec::new();
    let mut order: Vec<Ulid> = Vec::new();
    let mut comments: HashMap<Ulid, CommentState> = HashMap::new();

    for record in records {
        let seq = record.seq;
        match &record.body {
            // The header is the first record; later ones (which a valid session
            // never writes) do not displace it. Its version gates whether we can
            // safely interpret the rest of the log at all.
            RecordBody::Session(header) => {
                if session.is_none() {
                    if header.version > FORMAT_VERSION {
                        return Err(Error::UnsupportedVersion {
                            found: header.version,
                            supported: FORMAT_VERSION,
                        });
                    }
                    session = Some(header.clone());
                }
            }
            RecordBody::DiffVersion(version) => versions.push(version.clone()),
            RecordBody::Comment(comment) => {
                if !comments.contains_key(&comment.id) {
                    order.push(comment.id);
                }
                comments.insert(
                    comment.id,
                    CommentState {
                        id: comment.id,
                        author: comment.author.clone(),
                        target: comment.target.clone(),
                        version: comment.version,
                        anchor: comment.anchor.clone(),
                        body: comment.body.clone(),
                        resolved: false,
                        deleted: false,
                        confidence: None,
                        created_seq: seq,
                        updated_seq: seq,
                    },
                );
            }
            RecordBody::CommentEdit(edit) => {
                let comment = require_comment(&mut comments, edit.id, seq)?;
                comment.body = edit.body.clone();
                comment.updated_seq = seq;
            }
            RecordBody::CommentResolve(resolve) => {
                let comment = require_comment(&mut comments, resolve.id, seq)?;
                comment.resolved = resolve.resolved;
                comment.updated_seq = seq;
            }
            RecordBody::CommentDelete(delete) => {
                let comment = require_comment(&mut comments, delete.id, seq)?;
                comment.deleted = true;
                comment.updated_seq = seq;
            }
            RecordBody::CommentReanchor(reanchor) => {
                let comment = require_comment(&mut comments, reanchor.id, seq)?;
                comment.version = reanchor.version;
                comment.target = reanchor.target.clone();
                comment.confidence = Some(reanchor.confidence);
                comment.updated_seq = seq;
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

/// Look up the comment a mutation targets, treating a missing one as a corrupt
/// log rather than a no-op.
fn require_comment(
    comments: &mut HashMap<Ulid, CommentState>,
    id: Ulid,
    seq: u64,
) -> Result<&mut CommentState> {
    comments.get_mut(&id).ok_or_else(|| {
        Error::InconsistentLog(format!(
            "record at seq {seq} references unknown comment {id}"
        ))
    })
}
