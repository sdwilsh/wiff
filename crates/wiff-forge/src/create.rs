//! Opening a pull request for a reviewed branch that has no forge yet.
//!
//! `push --create` publishes the reviewed commit as a branch and opens a pull
//! request from it, so the reviewer never has to push and name a branch by hand
//! before wiff can mirror the review. The branch is named from the review's
//! description title, derived so the same review always publishes to the same
//! branch, with a suffix drawn from the session to keep two reviews whose titles
//! slugify alike from colliding on one remote branch. Preflight checks refuse
//! before anything is published, leaving the remote untouched when the review is
//! not ready to open.

use anyhow::Result;
use wiff_core::record::{ForgeUrl, RevisionId};
use wiff_core::source::ScmRepo;
use wiff_core::{ReviewState, SessionId};

use crate::Forge;
use crate::types::NewPullRequest;

/// The inputs for opening a pull request from a reviewed branch that has no
/// forge yet.
pub struct OpenRequest<'a> {
    /// The repository to open the pull request in.
    pub repo: &'a ForgeUrl,
    /// The local name of the remote to publish the branch to.
    pub remote: &'a str,
    /// The reviewed commit to publish as the pull request's head.
    pub head_commit: &'a RevisionId,
    /// The branch the pull request merges into.
    pub base_branch: &'a str,
}

/// A pull request opened by [`open_pull_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedPullRequest {
    /// The new pull request's URL.
    pub url: ForgeUrl,
    /// The branch published to hold the reviewed commit.
    pub branch: String,
}

/// A reason [`open_pull_request`] refuses, reported before anything is
/// published so the remote is left untouched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenRefusal {
    /// The working tree has changes that are not in the reviewed commit, so the
    /// published branch would not hold what was reviewed.
    #[error(
        "the working tree has uncommitted changes; commit or discard them before opening a pull request"
    )]
    DirtyWorkingTree,
    /// No description is set to name the branch and fill the pull request.
    #[error(
        "the review has no description to name the branch and fill the pull request; set one before opening it"
    )]
    NoDescription,
    /// The description's title has no letters or digits, so it can neither name
    /// a branch nor fill the pull request's title.
    #[error(
        "the description title {title:?} has no letters or digits to name a branch or fill the pull request; give it a usable title before opening it"
    )]
    UnusableTitle {
        /// The title that yielded no usable branch name.
        title: String,
    },
    /// The current branch already tracks an upstream other than the branch this
    /// would publish, so the local branch would keep tracking there rather than
    /// the published branch, and a later plain push would go to the wrong place.
    #[error(
        "the current branch already tracks {remote}/{branch}; a later push would go there rather than the pull request branch, so clear the upstream before opening it"
    )]
    UpstreamAlreadySet {
        /// The remote the current branch tracks.
        remote: String,
        /// The branch on that remote.
        branch: String,
    },
}

/// Publish the reviewed commit as a branch and open a pull request from it,
/// naming the branch from `state`'s description.
///
/// `req.head_commit` must be the tip of the checked-out branch: publishing
/// points that branch at the published one as its upstream, so a `head_commit`
/// that is not the current tip fails inside the SCM with a repository error
/// rather than an [`OpenRefusal`].
///
/// Refuses with [`OpenRefusal`], before any publish, when no description is set,
/// its title yields no usable branch name, the working tree is dirty, or the
/// checked-out branch already tracks an upstream other than the branch this
/// would publish. Reusing a branch that already holds `head_commit` lets a
/// re-run recover from an earlier partial failure rather than orphaning a branch
/// or opening a duplicate pull request.
pub async fn open_pull_request(
    forge: &dyn Forge,
    repo: &dyn ScmRepo,
    state: &ReviewState,
    req: &OpenRequest<'_>,
) -> Result<OpenedPullRequest> {
    let Some(description) = &state.description else {
        return Err(OpenRefusal::NoDescription.into());
    };
    let slug = branch_slug(&description.content.title);
    if slug.is_empty() {
        return Err(OpenRefusal::UnusableTitle {
            title: description.content.title.clone(),
        }
        .into());
    }
    if !repo.working_tree_is_clean().await? {
        return Err(OpenRefusal::DirtyWorkingTree.into());
    }

    // Reuse the plain slug across re-runs of the same review, including recovery
    // after an earlier run published the branch but failed to open the pull
    // request; disambiguate with a session-derived suffix only when the slug is
    // already taken on the remote by a different commit.
    let branch = match repo.remote_branch(req.remote, &slug).await? {
        Some(commit) if &commit != req.head_commit => disambiguated_branch(&slug, state.session.id),
        _ => slug,
    };

    // An upstream already pointing at the branch being published is the end
    // state publishing aims for -- a prior run set it -- not a blocker; one
    // pointing elsewhere would leave a later plain push going to the wrong
    // place, so refuse it.
    if let Some(upstream) = repo.current_upstream().await?
        && (upstream.remote != req.remote || upstream.branch != branch)
    {
        return Err(OpenRefusal::UpstreamAlreadySet {
            remote: upstream.remote,
            branch: upstream.branch,
        }
        .into());
    }

    repo.publish_branch(req.remote, &branch, req.head_commit)
        .await?;
    let url = forge
        .create_pull_request(&NewPullRequest {
            repo: req.repo.clone(),
            description: description.content.clone(),
            head_branch: branch.clone(),
            base_branch: req.base_branch.to_string(),
        })
        .await?;
    Ok(OpenedPullRequest { url, branch })
}

/// Turn a description title into a git branch name: lowercase, with each run of
/// characters that cannot appear in a readable branch name collapsed to a single
/// hyphen and the ends trimmed. A title that yields no usable characters (only
/// punctuation, say) returns an empty string, which the caller replaces with a
/// session-derived name.
pub fn branch_slug(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// Append the session's id to `slug`, for when the plain slug is already taken
/// on the remote by a different commit. The id is unique per session, so two
/// same-titled reviews get distinct branches. An empty `slug` (from a title with
/// no usable characters) becomes the id alone.
pub fn disambiguated_branch(slug: &str, session: SessionId) -> String {
    let suffix = session.to_string();
    if slug.is_empty() {
        suffix
    } else {
        format!("{slug}-{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_becomes_a_lowercase_hyphenated_slug() {
        wince::assert_eq!(branch_slug("Refactor the widget"), "refactor-the-widget");
    }

    #[test]
    fn punctuation_and_repeated_separators_collapse_to_single_hyphens() {
        wince::assert_eq!(
            branch_slug("  Fix: the parser (v2) -- again!  "),
            "fix-the-parser-v2-again"
        );
    }

    #[test]
    fn non_ascii_letters_are_dropped_rather_than_transliterated() {
        wince::assert_eq!(branch_slug("Cafe\u{301} au lait"), "cafe-au-lait");
    }

    #[test]
    fn a_title_with_no_usable_characters_yields_an_empty_slug() {
        wince::assert_eq!(branch_slug("--- !!! ---"), "");
    }

    #[test]
    fn a_disambiguated_branch_appends_the_id() {
        let session: SessionId = "0123456ab".parse().unwrap();
        wince::assert_eq!(
            disambiguated_branch("refactor-the-widget", session),
            "refactor-the-widget-0123456ab".to_string()
        );
    }

    #[test]
    fn a_disambiguated_empty_slug_is_the_id_alone() {
        let session: SessionId = "0123456ab".parse().unwrap();
        wince::assert_eq!(disambiguated_branch("", session), "0123456ab".to_string());
    }
}
