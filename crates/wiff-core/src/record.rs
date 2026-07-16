//! The record schema written to a session's append-only log.
//!
//! Every line of a session `.jsonl` is one [`Record`]: a sequence number, a
//! timestamp, and a tagged [`RecordBody`]. The `seq` is the record's 0-based
//! position in the file and its stable id within the session. Comment mutations
//! are append-only events under one [`CommentEvent`] envelope keyed by an
//! annotation [`Ulid`]; a comment's current state is the fold of its event
//! chain.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ulid::Ulid;
use wiff_diff::{FileStatus, LineNo, Side};

use crate::hash::SidebandHash;

/// The session format version, bumped when the record schema changes
/// incompatibly. A log whose header version differs from this, older or newer,
/// is refused rather than misread.
pub const FORMAT_VERSION: u32 = 3;

/// A record's 0-based position in its session log and its stable id within the
/// session. The same integer is a comment's `created_seq`/`updated_seq` and the
/// ordering key a later phase uses for verdict derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    /// The underlying integer.
    pub fn get(self) -> u64 {
        self.0
    }

    /// The next sequence number after this one.
    pub fn next(self) -> Self {
        Seq(self.0 + 1)
    }
}

impl std::fmt::Display for Seq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The number of a captured diff version, matching its sideband `vN.diff`. `v0`
/// is the first capture; each refresh increments it. A distinct type so a
/// version number is never confused with a sequence number or a raw count.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct VersionNumber(pub u32);

impl VersionNumber {
    /// The underlying integer.
    pub fn get(self) -> u32 {
        self.0
    }

    /// The next version number after this one.
    pub fn next(self) -> Self {
        VersionNumber(self.0 + 1)
    }
}

impl std::fmt::Display for VersionNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One line of a session log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The record's 0-based position in the file and stable id.
    pub seq: Seq,
    /// When the record was appended.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The record's payload.
    pub body: RecordBody,
}

/// The payload of a [`Record`]. An unrecognized `type` deserializes to
/// [`RecordBody::Unknown`] rather than failing outright, so a line can still be
/// read; folding then rejects it as corrupt, since a matching-version log never
/// writes one.
// The common variant (`CommentEvent`) is the large one; the rarer `Unknown`,
// `Session`, and `DiffVersion` are small. Boxing the large variant would move
// the hot path to the heap to shrink a cold one, so the size spread is kept.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordBody {
    /// The session header, always the first record.
    Session(SessionHeader),
    /// A captured diff snapshot.
    DiffVersion(DiffVersionRecord),
    /// One event in a comment's history (create, edit, resolve, delete,
    /// re-anchor).
    CommentEvent(CommentEvent),
    /// An unrecognized record type. A compatible-version log should never
    /// contain one, so folding rejects it as corrupt.
    #[serde(other)]
    Unknown,
}

/// The first record of a session: format version, project, and how the diff was
/// obtained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// The session's ULID (also its file name stem).
    pub ulid: Ulid,
    /// The record schema version.
    pub version: u32,
    /// The project bucket the session lives under.
    pub project: String,
    /// The repository root the project was derived from, if any.
    pub repo_root: Option<String>,
    /// The directory the session was created from.
    pub cwd: String,
    /// How the diff was captured, and whether it can be regenerated.
    pub source: SourceKind,
}

/// How a session's diff is obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceKind {
    /// `git diff` of the working tree against the index.
    GitWorktree,
    /// `git diff --cached` of the index against HEAD.
    GitIndex,
    /// `git show REF`: the changes a single revision introduces.
    GitRev {
        /// The revision to show, as the user named it (a ref, a sha, or `HEAD`).
        rev: String,
    },
    /// A unified diff read from stdin; not regenerable.
    Stdin,
}

impl SourceKind {
    /// Whether a new diff version can be captured for this source.
    pub fn regenerable(&self) -> bool {
        match self {
            SourceKind::GitWorktree | SourceKind::GitIndex | SourceKind::GitRev { .. } => true,
            SourceKind::Stdin => false,
        }
    }

    /// The stable identifier for this source, matching its serialized form.
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::GitWorktree => "git_worktree",
            SourceKind::GitIndex => "git_index",
            SourceKind::GitRev { .. } => "git_rev",
            SourceKind::Stdin => "stdin",
        }
    }
}

/// A captured diff snapshot. The raw diff text lives in the sideband
/// `vN.diff`; this record holds a lightweight index plus its content hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffVersionRecord {
    /// The version number, matching the sideband `vN.diff`.
    pub number: VersionNumber,
    /// The hash of the sideband diff text.
    pub diff_hash: SidebandHash,
    /// A per-file index of the captured diff.
    pub files: Vec<FileSummary>,
}

/// A lightweight index entry for one file in a diff version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSummary {
    /// The before-side path.
    pub old_path: String,
    /// The after-side path.
    pub new_path: String,
    /// How the file changed.
    pub status: FileStatus,
    /// The number of hunks in the file.
    pub hunk_count: u32,
}

/// Who authored an annotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    /// The author's display name.
    pub name: String,
    /// Whether the author is a human or an agent.
    pub kind: AuthorKind,
}

/// Whether an annotation's author is a human or an agent.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    /// A human reviewer.
    #[default]
    Human,
    /// An automated agent.
    Agent,
}

impl AuthorKind {
    /// The stable identifier for this author kind, matching its serialized form.
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthorKind::Human => "human",
            AuthorKind::Agent => "agent",
        }
    }
}

/// A forge instance: a provider family plus its normalized host, so two
/// installations of the same provider never collide. Kept as strings rather
/// than a closed enum so a new forge needs no schema change.
///
/// Unused until a later phase populates forge provenance; defined now so the
/// record schema is fixed at the format break.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForgeId {
    /// The provider family: "github", "gitlab", "codeberg", ...
    pub provider: String,
    /// The normalized host of this instance, so github.com and a self-hosted
    /// enterprise install are distinct: "github.com", "git.example.com".
    pub host: String,
}

/// The neutral class of forge object an [`ExternalRef`] names, translated per
/// adapter to the forge's own object types (a `ReviewComment` is a GitHub
/// review comment or a GitLab diff-note; a `Verdict` is a GitHub review
/// submission or a GitLab approval).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalKind {
    /// A review comment or discussion note.
    ReviewComment,
    /// The review description (a pull-request body or commit message).
    Description,
    /// A review-level verdict submission.
    Verdict,
}

/// A stable name for an object on a forge, kept opaque so a new forge needs no
/// schema change. `(forge, kind, id)` names the object; `url` is a presentation
/// link to it.
///
/// Unused until a later phase mirrors forge state; defined now so the record
/// schema is fixed at the format break.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalRef {
    /// The forge instance the object lives on.
    pub forge: ForgeId,
    /// What kind of object it is.
    pub kind: ExternalKind,
    /// The forge's identifier for the object, a string so it covers numeric ids
    /// and global-node ids alike.
    pub id: String,
    /// A link to the object, when the forge gives one. Presentation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// What an annotation is attached to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum CommentTarget {
    /// A line range within a file, on one side of the diff.
    Lines {
        /// The file's display path.
        file: String,
        /// Which side the lines are on.
        side: Side,
        /// The first line of the range.
        start_line: LineNo,
        /// The last line of the range, inclusive.
        end_line: LineNo,
    },
    /// A whole file.
    File {
        /// The file's display path.
        file: String,
    },
    /// The review overall.
    Review,
    /// A reply to another comment, identified by its parent's id. A reply has no
    /// anchor of its own.
    Comment {
        /// The comment being replied to.
        id: Ulid,
    },
}

/// The captured content a line-range comment rebases against: the anchored
/// lines plus a window of surrounding context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// The exact text of the anchored lines.
    pub snippet: Vec<String>,
    /// Context lines immediately before the snippet.
    pub context_before: Vec<String>,
    /// Context lines immediately after the snippet.
    pub context_after: Vec<String>,
}

/// How confidently a comment was re-anchored onto a newer diff version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// The anchored lines were found unchanged.
    Exact,
    /// The anchored lines were found shifted or slightly altered.
    Approximate,
    /// The anchored lines could not be confidently located; the comment is
    /// retained but flagged.
    Outdated,
}

/// One event in a comment's history. `id` names the comment the event applies
/// to; a [`CommentEventKind::Create`] introduces that id. The metadata common
/// to every mutation lives here rather than on each variant, so an edit and a
/// re-anchor record who and when the same way a resolve and a delete do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentEvent {
    /// The comment this event applies to.
    pub id: Ulid,
    /// Who performed the event.
    pub author: Author,
    /// When the event was authored, when that differs from when wiff recorded
    /// it. Absent for a locally originated event, whose time is the enclosing
    /// [`Record::at`]; present for an imported event, whose authoritative time
    /// is its time on the originating forge.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub authored_at: Option<OffsetDateTime>,
    /// Where the event came from, for a mirrored review. Absent for a local
    /// event. Unpopulated until a later phase mirrors forge state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ExternalRef>,
    /// The upstream version this event reconciled with, opaque and
    /// adapter-interpreted. Unpopulated until a later phase mirrors forge state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_marker: Option<String>,
    /// What the event does.
    #[serde(flatten)]
    pub kind: CommentEventKind,
}

/// What a [`CommentEvent`] does. Later phases add `SetDisposition` and `Link`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CommentEventKind {
    /// Introduce a new comment.
    Create(CommentCreate),
    /// Revise a comment's body.
    Edit {
        /// The new body text.
        body: String,
    },
    /// Change a comment's resolved state.
    Resolve {
        /// The new resolved state.
        resolved: bool,
    },
    /// Withdraw a comment (a tombstone).
    Delete,
    /// Re-anchor a comment onto a newer diff version.
    Reanchor(CommentReanchor),
}

/// The initial state of a comment, as its create event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentCreate {
    /// What the comment is attached to.
    pub target: CommentTarget,
    /// The diff version it was authored against.
    pub version: VersionNumber,
    /// The captured content for rebasing, for line-range targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    /// The comment text.
    pub body: String,
}

/// A re-anchoring of a comment onto a newer diff version. The comment's id lives
/// on the enclosing [`CommentEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentReanchor {
    /// The diff version it now anchors to.
    pub version: VersionNumber,
    /// Its target in the new version.
    pub target: CommentTarget,
    /// How confidently it was relocated.
    pub confidence: Confidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_source_round_trips_through_json() {
        let source = SourceKind::GitRev {
            rev: "HEAD".to_string(),
        };
        let json = serde_json::to_string(&source).expect("serialize");
        wince::assert_eq!(json, r#"{"kind":"git_rev","rev":"HEAD"}"#.to_string());
        let back: SourceKind = serde_json::from_str(&json).expect("deserialize");
        wince::assert_eq!(back, source);
        wince::assert_eq!(source.regenerable(), true);
        wince::assert_eq!(source.as_str(), "git_rev");
    }

    #[test]
    fn a_create_event_round_trips_through_json() {
        let event = CommentEvent {
            id: Ulid::nil(),
            author: Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            authored_at: None,
            origin: None,
            synced_marker: None,
            kind: CommentEventKind::Create(CommentCreate {
                target: CommentTarget::Review,
                version: VersionNumber(0),
                anchor: None,
                body: "looks good".to_string(),
            }),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        wince::assert_eq!(
            json,
            r#"{"id":"00000000000000000000000000","author":{"name":"wez","kind":"human"},"event":"create","target":{"target":"review"},"version":0,"body":"looks good"}"#
                .to_string()
        );
        let back: CommentEvent = serde_json::from_str(&json).expect("deserialize");
        wince::assert_eq!(back, event);
    }
}
