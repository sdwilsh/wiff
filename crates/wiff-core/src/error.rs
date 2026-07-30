//! Error types for the core session layer.

use std::path::PathBuf;

use ulid::Ulid;

use crate::record::CommentNumber;
use crate::session_id::SessionId;

/// The result type used throughout the core layer.
pub type Result<T> = std::result::Result<T, Error>;

/// An error from session storage, discovery, or identity resolution.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An underlying filesystem or IO failure.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path being operated on when the error occurred.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },

    /// A session record could not be serialized or deserialized.
    #[error("could not decode session record: {0}")]
    Decode(#[from] serde_json::Error),

    /// A diff source failed to produce a diff.
    #[error("could not capture diff: {0}")]
    Source(String),

    /// A repository operation failed: running the SCM, or a precondition it
    /// enforces (publishing a commit that is not the branch tip, pinning a base
    /// the repo does not hold). The message describes the failure on its own, so
    /// it is passed through without a framing prefix.
    #[error("{0}")]
    Repo(String),

    /// A stored base ruleset could not be parsed.
    #[error(transparent)]
    BaseRuleset(#[from] crate::base_ruleset::ParseError),

    /// Captured diff text could not be parsed into the diff model.
    #[error("could not parse captured diff: {0}")]
    Diff(#[from] wiff_diff::parse::ParseError),

    /// Another process holds the session's exclusive lock.
    #[error("session {0} is locked by another process")]
    Locked(PathBuf),

    /// The session file advanced past our in-memory position, so appending
    /// would duplicate a sequence number.
    #[error("session {0} diverged from our position")]
    Diverged(PathBuf),

    /// A session path did not name a valid session.
    #[error("{path} is not a valid session: {reason}")]
    NotASession {
        /// The offending path.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },

    /// The working directory is not inside a repository and no project name was
    /// forced.
    #[error("could not determine a project for {0}; pass an explicit project name")]
    NoProject(PathBuf),

    /// The platform data directory could not be resolved.
    #[error("could not determine the wiff data directory")]
    NoDataDir,

    /// No session matched a discovery request.
    #[error("no session found for project {0}")]
    NoSession(String),

    /// A session id or id prefix named no session in the project.
    #[error("no session in project {project} matches id {query:?}")]
    UnknownSessionId {
        /// The project directory searched.
        project: String,
        /// The id or prefix provided.
        query: String,
    },

    /// A session id prefix named more than one session; more characters are
    /// needed to pick one out.
    #[error("session id {query:?} is ambiguous; it matches {}", join_ids(.matches))]
    AmbiguousSessionId {
        /// The prefix provided.
        query: String,
        /// Every session whose id begins with the prefix, most recent first.
        matches: Vec<SessionId>,
    },

    /// A session was created under an id that already names one. Reachable only
    /// through the caller-chosen-id path, where a resync that re-imports a pull
    /// request can detect the existing session and reuse it rather than fail.
    #[error("session {0} already exists")]
    SessionExists(SessionId),

    /// A session's records lacked the leading header, so it cannot be folded.
    #[error("session has no header record")]
    MissingHeader,

    /// A comment was requested against a session that has captured no diff
    /// version yet, so there is nothing to author it against.
    #[error("session has no diff version to comment on")]
    NoDiffVersion,

    /// A comment's target does not correspond to content in the session's diff,
    /// so it cannot be anchored.
    #[error("cannot anchor comment: {0}")]
    Anchor(String),

    /// A mutation named a comment the session has no record of.
    #[error("no comment {0} in this session")]
    UnknownComment(Ulid),

    /// A reference named a review-scoped comment number with no matching
    /// comment in this session.
    #[error("no comment {0} in this session")]
    UnknownCommentNumber(CommentNumber),

    /// A reply named a comment that has been withdrawn. Fold still keeps a reply
    /// under a parent withdrawn elsewhere, but authoring a fresh reply to a
    /// comment already gone is refused locally.
    #[error("cannot reply to withdrawn comment {0}")]
    WithdrawnComment(Ulid),

    /// A verdict was set on a comment authored by someone else. A verdict is the
    /// comment author's own, so this is refused rather than written, which would
    /// make fold reject the whole log.
    #[error("cannot set a verdict on comment {0}, which you did not author")]
    ForeignDisposition(Ulid),

    /// A session was written by a format version that does not match this
    /// build's, older or newer, so it cannot be safely interpreted. wiff is
    /// pre-release with no migration: discard the session and re-capture.
    #[error("session format version {found} does not match supported version {supported}")]
    UnsupportedVersion {
        /// The version recorded in the session header.
        found: u32,
        /// The version this build reads and writes.
        supported: u32,
    },

    /// A session's records are internally inconsistent and cannot be folded.
    #[error("inconsistent session log: {0}")]
    InconsistentLog(String),

    /// A path could not be included in an explore review because it is not
    /// readable as line-oriented text: it is missing, or it is binary.
    #[error("cannot include {path} in the review: {reason}")]
    UnreadablePath {
        /// The offending path, in the review's own spelling.
        path: String,
        /// Why it could not be included.
        reason: String,
    },
}

/// Render session ids as a comma-separated list for the ambiguity message.
fn join_ids(ids: &[SessionId]) -> String {
    ids.iter()
        .map(SessionId::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Error {
    /// Build an [`Error::Io`] tagged with the `path` under operation.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}
