//! The record schema written to a session's append-only log.
//!
//! Every line of a session `.jsonl` is one [`Record`]: a sequence number, a
//! timestamp, and a tagged [`RecordBody`]. The `seq` is the record's 0-based
//! position in the file and its stable id within the session. Comment mutations
//! are append-only events keyed by an annotation [`Ulid`]; a comment's current
//! state is the fold of its event chain.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ulid::Ulid;
use wiff_diff::{FileStatus, LineNo, Side};

use crate::hash::SidebandHash;

/// The session format version, bumped when the record schema changes
/// incompatibly.
pub const FORMAT_VERSION: u32 = 1;

/// One line of a session log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The record's 0-based position in the file and stable id.
    pub seq: u64,
    /// When the record was appended.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The record's payload.
    pub body: RecordBody,
}

/// The payload of a [`Record`]. Unknown variants are skipped on read so the
/// format can grow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordBody {
    /// The session header, always the first record.
    Session(SessionHeader),
    /// A captured diff snapshot.
    DiffVersion(DiffVersionRecord),
    /// A new annotation.
    Comment(CommentRecord),
    /// A revision to a comment's body.
    CommentEdit(CommentEdit),
    /// A change to a comment's resolved state.
    CommentResolve(CommentResolve),
    /// A withdrawal of a comment (a tombstone).
    CommentDelete(CommentDelete),
    /// A re-anchoring of a comment onto a newer diff version.
    CommentReanchor(CommentReanchor),
    /// A revision to the overall review summary.
    ReviewSummary(ReviewSummary),
    /// An unrecognized record type written by a newer format; skipped on read.
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceKind {
    /// `git diff` of the working tree against the index.
    GitWorktree,
    /// `git diff --cached` of the index against HEAD.
    GitIndex,
    /// A unified diff read from stdin; not regenerable.
    Stdin,
}

impl SourceKind {
    /// Whether a new diff version can be captured for this source.
    pub fn regenerable(&self) -> bool {
        matches!(self, SourceKind::GitWorktree | SourceKind::GitIndex)
    }
}

/// A captured diff snapshot. The raw diff text lives in the sideband
/// `vN.diff`; this record holds a lightweight index plus its content hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffVersionRecord {
    /// The version number, matching the sideband `vN.diff`.
    pub number: u32,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    /// A human reviewer.
    Human,
    /// An automated agent.
    Agent,
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

/// A new annotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentRecord {
    /// The annotation's stable identity across edits.
    pub id: Ulid,
    /// Who wrote it.
    pub author: Author,
    /// What it is attached to.
    pub target: CommentTarget,
    /// The diff version it was authored against.
    pub version: u32,
    /// The captured content for rebasing, for line-range targets.
    pub anchor: Option<Anchor>,
    /// The comment text.
    pub body: String,
}

/// A revision to a comment's body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentEdit {
    /// The comment being edited.
    pub id: Ulid,
    /// The new body text.
    pub body: String,
}

/// A change to a comment's resolved state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentResolve {
    /// The comment being resolved or reopened.
    pub id: Ulid,
    /// The new resolved state.
    pub resolved: bool,
}

/// A withdrawal of a comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentDelete {
    /// The comment being withdrawn.
    pub id: Ulid,
}

/// A re-anchoring of a comment onto a newer diff version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentReanchor {
    /// The comment being moved forward.
    pub id: Ulid,
    /// The diff version it now anchors to.
    pub version: u32,
    /// Its target in the new version.
    pub target: CommentTarget,
    /// How confidently it was relocated.
    pub confidence: Confidence,
}

/// A revision to the overall review summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewSummary {
    /// Who wrote it.
    pub author: Author,
    /// The summary text.
    pub body: String,
}
