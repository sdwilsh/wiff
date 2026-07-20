//! The forge-neutral request and response shapes the [`Forge`](crate::Forge)
//! trait trades in. Each adapter translates its own wire forms into these, so
//! nothing forge-specific reaches the layers above.

use std::collections::BTreeMap;

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_core::record::{Author, Description, Disposition, ExternalRef, ForgeUrl, RevisionId};
use wiff_core::source::FetchSource;
use wiff_diff::{LineNo, Side};

/// A pull request as wiff mirrors it: the metadata read over the HTTP API,
/// together with the coordinates for bringing its commits into a local repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPullRequest {
    /// The pull request's canonical URL.
    pub url: ForgeUrl,
    /// The pull request's title and body.
    pub description: Description,
    /// How to bring the pull request's commits into a local repo; the wire
    /// protocol the head repository speaks picks the variant.
    pub head: FetchSource,
    /// The target branch the pull request merges into.
    pub base_ref: String,
    /// The target branch's resolved tip; the review's base is the merge-base of
    /// this commit with the head.
    pub base_commit: RevisionId,
    /// The pull request's inline and review-level comments.
    pub comments: Vec<FetchedComment>,
    /// The pull request's reviews, each with a verdict or a plain summary.
    pub reviews: Vec<FetchedReview>,
}

impl FetchedPullRequest {
    /// The head commit the review diffs against, held within [`Self::head`] as
    /// the commit its fetch resolves to.
    pub fn head_commit(&self) -> &RevisionId {
        self.head.commit()
    }
}

/// An inline position on a pull request's diff in the forge's terms, used in
/// both directions: where a fetched comment is anchored, and where a comment
/// being pushed should anchor. Both ends sit on one `side`; a forge that reports
/// a range whose ends fall on different sides of the diff is collapsed by the
/// adapter onto the end line's side. Callers uphold `start_line <= end_line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeAnchor {
    /// The changed file's path.
    pub path: String,
    /// Which side of the diff the anchor is on.
    pub side: Side,
    /// The first line of the anchored range.
    pub start_line: LineNo,
    /// The last line of the anchored range, inclusive.
    pub end_line: LineNo,
    /// The commit the anchor was recorded against. When it is not the current
    /// head, the comment may be outdated.
    pub commit: RevisionId,
}

/// A comment on a pull request as the forge reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedComment {
    /// The forge object this mirrors.
    pub origin: ExternalRef,
    /// Who wrote the comment.
    pub author: Author,
    /// The comment body.
    pub body: String,
    /// When the forge recorded the comment.
    pub authored_at: OffsetDateTime,
    /// Where the forge anchors the comment, or `None` for a review-level one.
    pub anchor: Option<ForgeAnchor>,
    /// The comment this replies to, for a threaded reply.
    pub reply_to: Option<ExternalRef>,
    /// The thread's resolution, when it is resolved.
    pub resolution: Option<Resolution>,
}

/// A comment thread's resolution, present on a [`FetchedComment`] when its
/// thread is resolved and absent when it is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Who resolved the thread; `None` when the forge reports it resolved
    /// without naming a resolver.
    pub by: Option<Author>,
}

/// A review submission on a pull request as the forge reports it: a verdict,
/// or a plain comment review with a summary body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedReview {
    /// The forge object this mirrors.
    pub origin: ExternalRef,
    /// Who submitted the review.
    pub author: Author,
    /// The review's summary body; may be empty.
    pub body: String,
    /// When the forge recorded the review.
    pub authored_at: OffsetDateTime,
    /// The review state mapped to a disposition, or `None` for a plain comment
    /// review with no verdict.
    pub disposition: Option<Disposition>,
    /// Whether the review was later dismissed. A dismissed review no longer
    /// counts toward the verdict; the verdict it held before dismissal is not
    /// recorded.
    pub dismissed: bool,
}

/// A batched review to submit: the inline comments that anchor, together with
/// the verdict and summary, posted as one all-or-nothing call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingReview {
    /// Submitted as the review state; `None` submits as a plain comment review.
    pub disposition: Option<Disposition>,
    /// The review's summary body.
    pub body: String,
    /// The fresh inline comments to post with the review, each anchored and none
    /// a reply. A review submission cannot express a reply, and a review-level
    /// comment that could not anchor inline is posted separately; the push layer
    /// routes both through [`Forge::post_comment`](crate::Forge::post_comment)
    /// rather than placing them here.
    pub comments: Vec<OutgoingComment>,
}

/// A single comment to publish, either within a batched [`OutgoingReview`] or
/// as a standalone post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingComment {
    /// The ULID of the local comment this publishes.
    pub comment: Ulid,
    /// The comment body as authored. Each adapter decides how to present it.
    pub body: String,
    /// The comment's own disposition, when it has one. How to express it -- a
    /// native review state or a tag in the body -- is the adapter's choice.
    pub disposition: Option<Disposition>,
    /// Where to anchor inline, or `None` to post at review level.
    pub anchor: Option<ForgeAnchor>,
    /// The forge object this replies to, for a reply to a linked comment.
    pub reply_to: Option<ExternalRef>,
}

/// The result of submitting an [`OutgoingReview`]: the forge objects the
/// submission created, keyed by the ULID of the local comment each publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedReview {
    /// The review object itself.
    pub review: ExternalRef,
    /// The created comment objects, keyed by the local comment each publishes.
    pub comments: BTreeMap<Ulid, ExternalRef>,
}

/// The details of a pull request to open from an already-pushed branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPullRequest {
    /// The repository to open the pull request in, e.g.
    /// `https://github.com/octo/demo`. An adapter is bound only to a host, so
    /// the repository is named here rather than derived from an existing pull
    /// request URL, which does not yet exist.
    pub repo: ForgeUrl,
    /// The pull request's title and body.
    pub description: Description,
    /// The branch holding the changes to review.
    pub head_branch: String,
    /// The branch the pull request merges into.
    pub base_branch: String,
}
