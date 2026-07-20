//! A forge-neutral interface to a pull request's host.
//!
//! wiff mirrors a pull request -- its diff, description, comments, and verdicts
//! -- and publishes a review back. Each forge (GitHub, Forgejo, and others)
//! speaks its own HTTP API; this crate defines the one trait that names those
//! operations and the request and response types they trade in, all in terms
//! wiff owns. An adapter is the only code that knows a forge's wire shapes,
//! keeping the layers above forge-agnostic. Sequencing a pull or a push is left
//! to the orchestration above this trait.

pub mod config;
pub mod error;
pub mod github;
pub mod pull;
pub mod types;

pub use config::{ForgeHost, ForgeTable, TokenOverride, resolve_token};
pub use error::Unsupported;
pub use github::GithubForge;
pub use pull::{reconcile_comments, reconcile_reviews};
pub use types::{
    FetchedComment, FetchedPullRequest, FetchedReview, ForgeAnchor, NewPullRequest,
    OutgoingComment, OutgoingReview, Resolution, SubmittedReview,
};

use anyhow::Result;
use async_trait::async_trait;
use wiff_core::record::{Description, ExternalRef, ForgeUrl};

/// One forge's HTTP operations, each roughly a single round-trip. An instance
/// is bound to a single host: a pull request is named by its [`ForgeUrl`] and a
/// comment or review object by its [`ExternalRef`], both on that host, and
/// [`create_pull_request`](Forge::create_pull_request) opens one there.
#[async_trait]
pub trait Forge: Send + Sync {
    /// Fetch a pull request and everything wiff mirrors from it: the coordinates
    /// for obtaining its diff, its description, review comments, and verdicts.
    async fn fetch(&self, pr: &ForgeUrl) -> Result<FetchedPullRequest>;

    /// Submit the batched review -- the inline comments that anchor, plus the
    /// verdict and summary -- as one all-or-nothing call.
    async fn submit_review(
        &self,
        pr: &ForgeUrl,
        review: &OutgoingReview,
    ) -> Result<SubmittedReview>;

    /// Post a standalone comment: a review-level fallback that could not anchor
    /// inline, or a reply to an existing thread.
    async fn post_comment(&self, pr: &ForgeUrl, comment: &OutgoingComment) -> Result<ExternalRef>;

    /// Re-publish an edit of a comment already linked to the forge.
    async fn edit_comment(&self, at: &ExternalRef, body: &str) -> Result<()>;

    /// Mark a linked comment's thread resolved or unresolved.
    async fn set_resolved(&self, at: &ExternalRef, resolved: bool) -> Result<()>;

    /// Update the pull request's title and body outside the batched review.
    async fn set_description(&self, pr: &ForgeUrl, description: &Description) -> Result<()>;

    /// Open a new pull request from an already-pushed branch.
    async fn create_pull_request(&self, req: &NewPullRequest) -> Result<ForgeUrl>;
}
