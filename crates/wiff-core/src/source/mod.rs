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

use std::path::Path;

use async_trait::async_trait;

use crate::error::Result;
use crate::identity::ScmType;
use crate::record::{RevisionId, SourceKind};

pub use git::{GitRepo, GitSource};

/// The branch the repository at `repo_root` currently has checked out, as a full
/// ref name (`refs/heads/...`), or `None` when the head is detached, the scm has
/// no such notion, or git cannot be reached. Session discovery uses this to
/// prefer the session that reviews the current branch.
pub fn current_branch(repo_root: &Path, scm: ScmType) -> Option<String> {
    match scm {
        ScmType::Git => git::current_branch(repo_root),
        ScmType::Jujutsu | ScmType::Sapling | ScmType::Mercurial => None,
    }
}

/// A diff captured from a source, ready to record as a review's next version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedDiff {
    /// The unified diff text.
    pub text: String,
    /// How the diff was obtained.
    pub source: SourceKind,
    /// The base commit the diff was captured against, when the source has an
    /// authoritative base.
    pub base_revision: Option<RevisionId>,
    /// Whether the resolved base is anchored to the tip under review (e.g. via
    /// `parent(@)`), and so expected to move with it. Meaningful only when
    /// `base_revision` is set.
    pub base_tip_relative: bool,
    /// The tip commit the diff was captured at, absent for a working-tree or
    /// index capture whose tip is the uncommitted state.
    pub head_revision: Option<RevisionId>,
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
