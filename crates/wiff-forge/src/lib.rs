//! A forge-neutral interface to a pull request's host.
//!
//! wiff mirrors a pull request -- its diff, description, comments, and verdicts
//! -- and publishes a review back. Each forge (GitHub, Forgejo, and others)
//! speaks its own HTTP API; this crate defines the one trait that names those
//! operations and the request and response types they trade in, all in terms
//! wiff owns. An adapter is the only code that knows a forge's wire shapes,
//! keeping the layers above forge-agnostic. Sequencing a pull or a push is left
//! to the orchestration above this trait.

pub mod apply;
pub mod blob_diff;
pub mod clone_url;
pub mod config;
pub mod create;
pub mod error;
pub mod github;
pub mod import;
pub mod pull;
pub mod push;
pub mod remote;
pub mod resync;
pub mod types;

pub use apply::{DeclinedWrite, PushOutcome, push};
pub use blob_diff::{ChangedFile, Content, assemble_diff};
pub use config::{ForgeHost, ForgeTable, TokenOverride, resolve_token};
pub use create::{
    OpenRefusal, OpenRequest, OpenedPullRequest, branch_slug, disambiguated_branch,
    open_pull_request,
};
pub use error::Unsupported;
pub use github::GithubForge;
pub use import::{ImportOutcome, ImportRequest, import_pull_request};
pub use pull::{reconcile_comments, reconcile_description, reconcile_reviews};
pub use push::{PlanError, PushEdit, PushPlan, PushResolve, plan_push};
pub use remote::select_pull_request_remote;
pub use resync::{ResyncOutcome, resync_pull_request};
pub use types::{
    FetchedComment, FetchedDescription, FetchedPullRequest, FetchedReview, ForgeAnchor,
    NewPullRequest, OutgoingComment, OutgoingReview, Resolution, SubmittedReview,
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

    /// Fetch every changed file of the pull request with the base and head
    /// contents needed to assemble its diff, for when there is no local clone to
    /// diff against.
    async fn fetch_changed_files(&self, pr: &ForgeUrl) -> Result<Vec<ChangedFile>>;

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

    /// Build the canonical web URL of the pull request `id` in the repository
    /// the clone URL `remote_url` points at. `id` is opaque: a forge names its
    /// pull requests however it likes, wiff never parses it, and it is
    /// percent-encoded into the URL as-is.
    fn pull_request_url(&self, remote_url: &str, id: &str) -> Result<ForgeUrl>;

    /// The project bucket a repo-less review of `pr` belongs to: a stable name
    /// read from the pull request URL's host, owner, and repository, grouping
    /// every repo-less review of one repository together.
    fn project_bucket(&self, pr: &ForgeUrl) -> Result<String>;

    /// Whether `remote_url`, a git clone URL, addresses the same repository the
    /// pull request `pr` lives in: the same host, owner, and repository under
    /// the forge's own equivalence (letter case, and the `.git` suffix). A clone
    /// URL this forge cannot read as one of its repositories does not match.
    fn matches_remote(&self, pr: &ForgeUrl, remote_url: &str) -> Result<bool>;
}
