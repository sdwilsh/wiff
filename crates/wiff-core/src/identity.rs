//! Resolving the project a session belongs to.
//!
//! A session is bucketed by its repository root, so every checkout of the same
//! working tree shares a bucket and an agent can find the right session for a
//! directory in one step. The bucket's [`canonical`](ProjectIdentity::canonical)
//! name is derived deterministically from the repo root path; a caller may also
//! force an explicit name for input that is not inside a repository.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The source-control system a repository root belongs to, named by the short
/// token it is written with everywhere the model refers to it: the `git:` gate
/// on a base rule and the `git(...)` scm-native escape hatch. The serde rename
/// on each variant fixes the on-disk form to that token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScmType {
    /// A git checkout.
    #[serde(rename = "git")]
    Git,
    /// A Jujutsu workspace.
    #[serde(rename = "jj")]
    Jujutsu,
    /// A Mercurial repository.
    #[serde(rename = "hg")]
    Mercurial,
    /// A Sapling repository.
    #[serde(rename = "sl")]
    Sapling,
}

impl ScmType {
    /// Every scm wiff knows, in a stable order for listing the accepted tokens
    /// in a message.
    pub const ALL: [ScmType; 4] = [
        ScmType::Git,
        ScmType::Jujutsu,
        ScmType::Mercurial,
        ScmType::Sapling,
    ];

    /// Read an scm from its short token, or `None` when the token names no known
    /// scm.
    pub fn from_token(token: &str) -> Option<Self> {
        serde_plain::from_str(token).ok()
    }
}

// `Display` and `from_token` read the serde token so the one rename on each
// variant is the single source of truth for both directions. The short token is
// the wire form deliberately: the model names an scm one way in messages, gates,
// and storage alike.
serde_plain::derive_display_from_serialize!(ScmType);

/// The project a session belongs to: the bucket name and the repository root it
/// was derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    /// The filesystem-safe bucket name (a directory under `sessions/`).
    pub canonical: String,
    /// The repository root the project was derived from, when one was found.
    pub repo_root: Option<PathBuf>,
    /// The source-control system of the repository root, when one was found.
    pub scm: Option<ScmType>,
}

impl ProjectIdentity {
    /// Resolve the project for `cwd` by locating its enclosing repository root.
    /// Fails with [`Error::NoProject`] when `cwd` is not inside a repository.
    pub fn for_dir(cwd: &Path) -> Result<Self> {
        let (repo_root, scm) =
            find_repo_root(cwd).ok_or_else(|| Error::NoProject(cwd.to_path_buf()))?;
        Ok(Self {
            canonical: canonical_for_root(&repo_root),
            repo_root: Some(repo_root),
            scm: Some(scm),
        })
    }

    /// Resolve the project for `cwd`. A `forced` name always wins as the bucket,
    /// overriding any name derived from the repository, so a caller can pin a
    /// session to a chosen project regardless of where it runs. Any repository
    /// root found is still recorded. Without a `forced` name the project is
    /// derived from the repository, and it is an error to have neither.
    pub fn for_dir_or_forced(cwd: &Path, forced: Option<&str>) -> Result<Self> {
        match (find_repo_root(cwd), forced) {
            (Some((repo_root, scm)), Some(name)) => Ok(Self {
                canonical: sanitize(name),
                repo_root: Some(repo_root),
                scm: Some(scm),
            }),
            (Some((repo_root, scm)), None) => Ok(Self {
                canonical: canonical_for_root(&repo_root),
                repo_root: Some(repo_root),
                scm: Some(scm),
            }),
            (None, Some(name)) => Ok(Self {
                canonical: sanitize(name),
                repo_root: None,
                scm: None,
            }),
            (None, None) => Err(Error::NoProject(cwd.to_path_buf())),
        }
    }

    /// Resolve a repo-less project from a forge-derived `bucket` name, for a
    /// session mirroring a pull request with no local checkout. `bucket` is
    /// sanitized into a safe single path component.
    pub fn for_forge(bucket: &str) -> Self {
        Self {
            canonical: sanitize(bucket),
            repo_root: None,
            scm: None,
        }
    }
}

/// The control directories that mark a workspace root, so discovery does not
/// assume git, paired with the SCM each identifies.
const WORKSPACE_MARKERS: [(&str, ScmType); 4] = [
    (".jj", ScmType::Jujutsu),
    (".git", ScmType::Git),
    (".sl", ScmType::Sapling),
    (".hg", ScmType::Mercurial),
];

/// Walk up from `start` looking for the nearest directory that holds one of the
/// [`WORKSPACE_MARKERS`] (the marker may be a directory or, for git worktrees
/// and submodules, a file), reporting the root and which SCM it belongs to.
fn find_repo_root(start: &Path) -> Option<(PathBuf, ScmType)> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        for (marker, scm) in WORKSPACE_MARKERS {
            if current.join(marker).exists() {
                return Some((current.to_path_buf(), scm));
            }
        }
        dir = current.parent();
    }
    None
}

/// Derive a stable, filesystem-safe bucket name from a repo root: its final
/// component plus a short digest of the absolute path, so distinct checkouts
/// with the same basename do not collide.
fn canonical_for_root(root: &Path) -> String {
    let base = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string());
    let digest = blake3::hash(root.to_string_lossy().as_bytes());
    let short = &digest.to_hex()[..8];
    format!("{}-{}", sanitize(&base), short)
}

/// Reduce `name` to a safe single path component: any character that is not
/// alphanumeric, `-`, `_`, or `.` becomes `_`.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "project".to_string()
    } else {
        cleaned
    }
}
