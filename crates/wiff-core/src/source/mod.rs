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
use ulid::Ulid;

use crate::error::Result;
use crate::identity::ScmType;
use crate::record::{RevisionId, SourceKind};

pub use git::{GitRepo, GitSource};

/// The checked-out branch of a repository, distinguishing a real detached head
/// from a transient failure to reach the scm. A detached head is a repository
/// state a caller can act on, while an unreachable scm reports nothing about the
/// branch at all; collapsing the two would let a momentary fault masquerade as a
/// detached head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadBranch {
    /// Checked out on this branch, named by its full ref (`refs/heads/...`).
    On(String),
    /// The head is detached, on no branch.
    Detached,
    /// The scm could not be reached, or gave an answer that could not be read.
    Unknown,
}

/// The branch state of the repository at `repo_root`. Any scm other than git,
/// which has no such notion here, reports [`HeadBranch::Unknown`].
pub fn head_branch(repo_root: &Path, scm: ScmType) -> HeadBranch {
    match scm {
        ScmType::Git => git::head_branch(repo_root),
        ScmType::Jujutsu | ScmType::Sapling | ScmType::Mercurial => HeadBranch::Unknown,
    }
}

/// The branch the repository at `repo_root` currently has checked out, as a full
/// ref name (`refs/heads/...`), or `None` when the head is detached, the scm has
/// no such notion, or git cannot be reached. Session discovery uses this to
/// prefer the session that reviews the current branch, where a detached head and
/// an unreachable scm are alike in offering no branch to match.
pub fn current_branch(repo_root: &Path, scm: ScmType) -> Option<String> {
    match head_branch(repo_root, scm) {
        HeadBranch::On(name) => Some(name),
        HeadBranch::Detached | HeadBranch::Unknown => None,
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

/// How to bring a pull request's commits into a local repo. The variant names
/// the wire protocol the forge's repository speaks, not the tool that runs
/// locally: a git working copy and a jj working copy both satisfy
/// [`FetchSource::Git`], the former by shelling out to git and the latter
/// through its git backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchSource {
    /// Fetch `git_ref` from `url` and expect it to resolve to `commit`. The
    /// forge adapter has already chosen between the pull-ref namespace and the
    /// head repository, so this holds one resolved fetch either way.
    Git {
        /// The repository the forge adapter chose to fetch from.
        url: String,
        /// The ref within that repository to fetch.
        git_ref: String,
        /// The commit the fetched ref is required to resolve to; a fetch that
        /// resolves to a different commit is rejected.
        commit: RevisionId,
    },
}

impl FetchSource {
    /// The commit this fetch is required to resolve to.
    pub fn commit(&self) -> &RevisionId {
        match self {
            FetchSource::Git { commit, .. } => commit,
        }
    }
}

/// Local repository operations forge support needs beyond producing diff text:
/// fetching a forge's commits, publishing a branch, and managing pins.
#[async_trait]
pub trait ScmRepo {
    /// Fetch `source` into the local repo, pin the fetched commit under the
    /// session's `head` pin, and return the commit it resolved to. Fails when
    /// this SCM cannot speak the protocol `source` names, or when the fetched
    /// ref does not resolve to the commit `source` expects.
    async fn fetch_pinned(&self, source: &FetchSource, session: Ulid) -> Result<RevisionId>;

    /// Pin an already-present `commit` under the session's `base` pin. The base
    /// is the pull request's target-branch tip, which the repo usually already
    /// holds.
    async fn pin_base(&self, commit: &RevisionId, session: Ulid) -> Result<()>;

    /// Publish `commit` to `remote` (the local name of the repository's remote
    /// for the forge host) as branch `branch`. Records tracking to the published
    /// branch when the checked-out branch has no upstream, leaving an existing
    /// upstream as the user configured it.
    async fn publish_branch(&self, remote: &str, branch: &str, commit: &RevisionId) -> Result<()>;

    /// Delete the session's pins, letting the fetched commits be garbage
    /// collected. The session's own files are untouched; discarding a session
    /// calls this to leave nothing behind in the repo. A pin that is already
    /// absent is not an error.
    async fn remove_pins(&self, session: Ulid) -> Result<()>;
}
