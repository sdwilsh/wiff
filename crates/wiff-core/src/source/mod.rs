//! Where a session's diff comes from.
//!
//! A [`DiffSource`] produces a unified diff as text, tagged with the
//! [`SourceKind`] that records how it was obtained and whether it can be
//! regenerated later. Capture is async so a source can do IO (running a
//! subprocess now, reaching a remote forge later) without blocking. Each
//! concrete source lives in its own submodule; v0 ships [`git`]. A diff already
//! in hand is itself a source: [`CapturedDiff`] implements [`DiffSource`] by
//! yielding a clone of itself, so a diff obtained by any means (piped on stdin,
//! read from a file, fetched over RPC) integrates through the same trait.

pub mod git;

use async_trait::async_trait;

use crate::error::Result;
use crate::record::SourceKind;

pub use git::GitSource;

/// A diff captured from a source: its unified diff text and how it was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedDiff {
    /// The unified diff text.
    pub text: String,
    /// How the diff was obtained.
    pub source: SourceKind,
}

/// A producer of unified diff text.
#[async_trait]
pub trait DiffSource {
    /// Capture the current diff from this source.
    async fn capture(&self) -> Result<CapturedDiff>;
}

#[async_trait]
impl DiffSource for CapturedDiff {
    async fn capture(&self) -> Result<CapturedDiff> {
        Ok(self.clone())
    }
}
