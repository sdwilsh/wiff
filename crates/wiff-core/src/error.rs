//! Error types for the core session layer.

use std::path::PathBuf;

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

    /// A session's records lacked the leading header, so it cannot be folded.
    #[error("session has no header record")]
    MissingHeader,

    /// A session was written by a newer format than this build understands, so
    /// it cannot be safely interpreted.
    #[error("session format version {found} is newer than supported version {supported}")]
    UnsupportedVersion {
        /// The version recorded in the session header.
        found: u32,
        /// The newest version this build understands.
        supported: u32,
    },

    /// A session's records are internally inconsistent and cannot be folded.
    #[error("inconsistent session log: {0}")]
    InconsistentLog(String),
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
