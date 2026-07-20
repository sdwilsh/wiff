//! The GitHub forge adapter, built on `octocrab`. It translates octocrab's
//! typed REST models into wiff's neutral shapes and converts their `chrono`
//! timestamps to `time`, keeping every GitHub-specific type within this module.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use octocrab::Octocrab;
use serde::de::IgnoredAny;
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tracing::warn;
use url::Url;
use wiff_core::record::{
    Author, AuthorKind, Description, ExternalKind, ExternalRef, ForgeId, ForgeUrl, RevisionId,
};
use wiff_core::source::FetchSource;
use wiff_diff::{LineNo, Side};

use crate::types::{FetchedComment, FetchedPullRequest, FetchedReview, ForgeAnchor, Resolution};

/// GitHub forge adapter backed by an `octocrab` client. One instance talks to a
/// single host, chosen by the API base the client was built with.
pub struct GithubForge {
    crab: Octocrab,
}

impl GithubForge {
    /// Build an adapter authenticating with `token`. `api_base` is the API root
    /// for a self-hosted instance, such as `https://github.example.com/api/v3`;
    /// `None` talks to public GitHub.
    pub fn new(token: &str, api_base: Option<&str>) -> Result<Self> {
        let mut builder = Octocrab::builder().personal_token(token.to_string());
        if let Some(base) = api_base {
            builder = builder
                .base_uri(base)
                .with_context(|| format!("{base} is not a valid GitHub API base URL"))?;
        }
        let crab = builder.build().context("building the GitHub API client")?;
        Ok(Self { crab })
    }

    /// Fetch the pull request `pr` names and everything wiff mirrors from it.
    pub async fn fetch(&self, pr: &ForgeUrl) -> Result<FetchedPullRequest> {
        let at = PullRequestId::parse(pr)?;
        let forge = ForgeId {
            provider: "github".to_string(),
            host: pr.host(),
        };

        // The metadata, comments, and reviews are independent reads, so a large
        // pull request need not pay their latencies in series.
        let metadata = async {
            self.crab
                .pulls(&at.owner, &at.repo)
                .get(at.number)
                .await
                .with_context(|| format!("fetching {pr}"))
        };
        let core = async {
            tokio::try_join!(
                metadata,
                self.fetch_comments(&at, &forge),
                self.fetch_reviews(&at, &forge),
            )
        };
        // Thread resolution reads over GraphQL, which a GitHub Enterprise host
        // may not expose and a REST-only token may not reach. It is additive
        // metadata, so its failure warns and degrades to no resolutions rather
        // than aborting a fetch that REST alone can satisfy.
        let (core, resolutions) = tokio::join!(core, self.fetch_thread_resolutions(&at));
        let (meta, mut comments, reviews) = core?;
        let resolutions = resolutions.unwrap_or_else(|err| {
            warn!("reading thread resolution for {pr} failed, importing without it: {err:#}");
            HashMap::new()
        });
        apply_resolutions(&mut comments, &resolutions);
        let head = head_source(pr, &at, &meta);

        Ok(FetchedPullRequest {
            url: pr.clone(),
            description: Description {
                title: meta.title.unwrap_or_default(),
                body: meta.body.unwrap_or_default(),
            },
            head,
            base_ref: meta.base.ref_field.clone(),
            base_commit: RevisionId(meta.base.sha.clone()),
            comments,
            reviews,
        })
    }

    /// Read the pull request's inline review comments and its review-level
    /// issue comments, mapped to the neutral shape.
    async fn fetch_comments(
        &self,
        at: &PullRequestId,
        forge: &ForgeId,
    ) -> Result<Vec<FetchedComment>> {
        // The inline and issue comment listings are independent, so they run
        // together rather than one after the other.
        let inline = async {
            let first = self
                .crab
                .pulls(&at.owner, &at.repo)
                .list_comments(Some(at.number))
                .send()
                .await
                .context("listing review comments")?;
            self.crab
                .all_pages(first)
                .await
                .context("listing review comments")
        };
        let issue = async {
            let first = self
                .crab
                .issues(&at.owner, &at.repo)
                .list_comments(at.number)
                .send()
                .await
                .context("listing issue comments")?;
            self.crab
                .all_pages(first)
                .await
                .context("listing issue comments")
        };
        let (inline, issue) = tokio::try_join!(inline, issue)?;

        let mut comments = Vec::with_capacity(inline.len() + issue.len());
        for comment in inline {
            comments.push(inline_comment(comment, forge)?);
        }
        for comment in issue {
            comments.push(issue_comment(comment, forge)?);
        }
        Ok(comments)
    }

    /// Read the pull request's submitted reviews, mapped to the neutral shape.
    /// A pending review, which the author has not yet submitted, is skipped.
    async fn fetch_reviews(
        &self,
        at: &PullRequestId,
        forge: &ForgeId,
    ) -> Result<Vec<FetchedReview>> {
        let pulls = self.crab.pulls(&at.owner, &at.repo);
        let reviews = pulls
            .list_reviews(at.number)
            .send()
            .await
            .context("listing reviews")?;
        let reviews = self
            .crab
            .all_pages(reviews)
            .await
            .context("listing reviews")?;

        let mut mapped = Vec::with_capacity(reviews.len());
        for review in reviews {
            if let Some(review) = fetched_review(review, forge)? {
                mapped.push(review);
            }
        }
        Ok(mapped)
    }

    /// Read each review thread's resolution state, keyed by the database id of
    /// its root comment. Only resolved threads appear; a thread's resolution
    /// attaches to its root comment, the anchor its replies hang off. The REST
    /// API exposes no thread state, so this reads over GraphQL.
    async fn fetch_thread_resolutions(
        &self,
        at: &PullRequestId,
    ) -> Result<HashMap<u64, Resolution>> {
        let threads = self.list_review_threads(at).await?;
        let mut resolved = HashMap::new();
        for thread in threads {
            if !thread.is_resolved {
                continue;
            }
            let Some(root) = thread.comments.nodes.first() else {
                continue;
            };
            let Some(database_id) = root.database_id else {
                continue;
            };
            let by = thread.resolved_by.map(actor_author);
            resolved.insert(database_id, Resolution { by });
        }
        Ok(resolved)
    }

    /// Mark the review thread that holds a linked comment resolved or
    /// unresolved. GitHub's mutation accepts only a thread node id, so the
    /// thread is found by matching the comment's database id against the
    /// comments of each review thread. Any comment locates its thread, root or
    /// reply, matching wiff's per-comment resolve against GitHub's per-thread
    /// state.
    pub async fn set_resolved(&self, at: &ExternalRef, resolved: bool) -> Result<()> {
        let web_url = at
            .url
            .as_deref()
            .context("a linked comment has no URL to locate its pull request")?;
        let forge_url = ForgeUrl::parse(web_url)
            .with_context(|| format!("{web_url} is not a usable comment URL"))?;
        let pr = PullRequestId::parse(&forge_url)?;
        let comment_id: u64 = at
            .id
            .parse()
            .with_context(|| format!("{} is not a github comment id", at.id))?;

        let threads = self.list_review_threads(&pr).await?;
        let thread_id = self
            .thread_holding(&threads, comment_id)
            .await?
            .with_context(|| format!("no review thread holds comment {comment_id}"))?;

        let mutation = if resolved {
            RESOLVE_THREAD_MUTATION
        } else {
            UNRESOLVE_THREAD_MUTATION
        };
        let _: IgnoredAny = self
            .crab
            .graphql(&json!({
                "query": mutation,
                "variables": { "threadId": thread_id },
            }))
            .await
            .with_context(|| format!("setting thread resolution for comment {comment_id}"))?;
        Ok(())
    }

    /// Find the node id of the review thread whose comments include
    /// `comment_id`. The first page of each thread's comments is already in
    /// `threads`; a thread whose comments overflow one page is paged through
    /// only when the id was not found on any first page, so the common match
    /// costs no extra request. Returns `None` when no thread holds the comment.
    async fn thread_holding(
        &self,
        threads: &[graphql::ReviewThread],
        comment_id: u64,
    ) -> Result<Option<String>> {
        for thread in threads {
            if thread.comments.holds(comment_id) {
                return Ok(Some(thread.id.clone()));
            }
        }
        for thread in threads {
            if !thread.comments.page_info.has_next_page {
                continue;
            }
            let mut cursor = thread.comments.page_info.end_cursor.clone();
            while let Some(after) = cursor {
                let page = self.thread_comment_page(&thread.id, &after).await?;
                if page.holds(comment_id) {
                    return Ok(Some(thread.id.clone()));
                }
                cursor = page.page_info.next_cursor();
            }
        }
        Ok(None)
    }

    /// Read one page of a review thread's comments past `cursor`, addressing the
    /// thread by its node id.
    async fn thread_comment_page(
        &self,
        thread_id: &str,
        cursor: &str,
    ) -> Result<graphql::CommentConnection> {
        let data: graphql::ThreadNodeData = self
            .crab
            .graphql(&json!({
                "query": THREAD_COMMENTS_QUERY,
                "variables": { "threadId": thread_id, "cursor": cursor },
            }))
            .await
            .context("paging a review thread's comments")?;
        Ok(data.node.comments)
    }

    /// Read every review thread on the pull request, following GraphQL's cursor
    /// pagination to the end.
    async fn list_review_threads(&self, at: &PullRequestId) -> Result<Vec<graphql::ReviewThread>> {
        let mut cursor: Option<String> = None;
        let mut threads = Vec::new();
        loop {
            let data: graphql::ThreadsData = self
                .crab
                .graphql(&json!({
                    "query": REVIEW_THREADS_QUERY,
                    "variables": {
                        "owner": at.owner,
                        "repo": at.repo,
                        "number": at.number,
                        "cursor": cursor,
                    },
                }))
                .await
                .context("listing review threads")?;
            let page = data.repository.pull_request.review_threads;
            threads.extend(page.nodes);
            if !page.page_info.has_next_page {
                break;
            }
            match page.page_info.end_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(threads)
    }
}

/// A pull request's coordinates on GitHub, taken from the path of its web URL,
/// `/<owner>/<repo>/pull/<number>`.
struct PullRequestId {
    owner: String,
    repo: String,
    number: u64,
}

impl PullRequestId {
    /// Read the owner, repository, and number from `pr`.
    fn parse(pr: &ForgeUrl) -> Result<Self> {
        let url = Url::parse(pr.as_str()).with_context(|| format!("parsing {pr}"))?;
        let mut segments = url.path_segments().into_iter().flatten();
        let owner = segments.next().unwrap_or_default();
        let repo = segments.next().unwrap_or_default();
        let kind = segments.next().unwrap_or_default();
        let number = segments.next().unwrap_or_default();
        if owner.is_empty() || repo.is_empty() || kind != "pull" {
            bail!("{pr} is not a github pull request URL");
        }
        let number = number
            .parse()
            .with_context(|| format!("{pr} has no pull request number"))?;
        Ok(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        })
    }
}

/// Build the fetch that brings the pull request's commits into a local repo.
/// GitHub serves every pull request under `refs/pull/<number>/head` on the base
/// repository, which reaches a fork without adding a remote for it. The base
/// repository's `clone_url` names where to fetch from; when the API omits it,
/// the URL is reconstructed from the pull request's own scheme and authority.
fn head_source(
    pr: &ForgeUrl,
    at: &PullRequestId,
    meta: &octocrab::models::pulls::PullRequest,
) -> FetchSource {
    let url = meta
        .base
        .repo
        .as_ref()
        .and_then(|repo| repo.clone_url.as_ref())
        .map(Url::to_string)
        .unwrap_or_else(|| fallback_clone_url(pr, at));
    FetchSource::Git {
        url,
        git_ref: format!("refs/pull/{}/head", meta.number),
        commit: RevisionId(meta.head.sha.clone()),
    }
}

/// Reconstruct the base repository's git URL from the pull request URL, keeping
/// its scheme and authority so a self-hosted instance on a non-default port is
/// still reached.
fn fallback_clone_url(pr: &ForgeUrl, at: &PullRequestId) -> String {
    let (scheme, authority) = Url::parse(pr.as_str())
        .ok()
        .and_then(|url| {
            let host = url.host_str()?.to_string();
            let authority = match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            };
            Some((url.scheme().to_string(), authority))
        })
        .unwrap_or_else(|| ("https".to_string(), pr.host()));
    format!("{scheme}://{authority}/{}/{}.git", at.owner, at.repo)
}

/// Translate an inline review comment, anchored to a position on the diff.
fn inline_comment(
    comment: octocrab::models::pulls::Comment,
    forge: &ForgeId,
) -> Result<FetchedComment> {
    let anchor = anchor_of(&comment)?;
    let reply_to = comment.in_reply_to_id.map(|id| ExternalRef {
        forge: forge.clone(),
        kind: ExternalKind::ReviewComment,
        id: id.to_string(),
        url: None,
    });
    Ok(FetchedComment {
        origin: object_ref(
            forge,
            ExternalKind::ReviewComment,
            comment.id.to_string(),
            Some(comment.html_url.clone()),
        ),
        author: author_of(comment.user.as_ref()),
        body: comment.body,
        authored_at: to_time(
            comment.created_at.timestamp(),
            comment.created_at.timestamp_subsec_nanos(),
        )?,
        anchor: Some(anchor),
        reply_to,
        resolution: None,
    })
}

/// Translate a review-level issue comment, which anchors nowhere on the diff.
fn issue_comment(
    comment: octocrab::models::issues::Comment,
    forge: &ForgeId,
) -> Result<FetchedComment> {
    Ok(FetchedComment {
        origin: object_ref(
            forge,
            ExternalKind::ReviewComment,
            comment.id.to_string(),
            Some(comment.html_url.to_string()),
        ),
        author: author_of(Some(&comment.user)),
        body: comment.body.unwrap_or_default(),
        authored_at: to_time(
            comment.created_at.timestamp(),
            comment.created_at.timestamp_subsec_nanos(),
        )?,
        anchor: None,
        reply_to: None,
        resolution: None,
    })
}

/// Map a submitted review to the neutral shape, returning `None` for one still
/// pending. A dismissed review reports no disposition, since GitHub overwrites
/// the review state on dismissal.
fn fetched_review(
    review: octocrab::models::pulls::Review,
    forge: &ForgeId,
) -> Result<Option<FetchedReview>> {
    use octocrab::models::pulls::ReviewState;
    use wiff_core::record::Disposition;

    let state = review.state;
    if matches!(state, Some(ReviewState::Pending) | None) {
        return Ok(None);
    }
    let Some(submitted_at) = review.submitted_at else {
        return Ok(None);
    };
    let disposition = match state {
        Some(ReviewState::Approved) => Some(Disposition::Approve),
        Some(ReviewState::ChangesRequested) => Some(Disposition::RequestChanges),
        _ => None,
    };
    Ok(Some(FetchedReview {
        origin: object_ref(
            forge,
            ExternalKind::Verdict,
            review.id.to_string(),
            Some(review.html_url.to_string()),
        ),
        author: author_of(review.user.as_ref()),
        body: review.body.unwrap_or_default(),
        authored_at: to_time(
            submitted_at.timestamp(),
            submitted_at.timestamp_subsec_nanos(),
        )?,
        disposition,
        dismissed: matches!(state, Some(ReviewState::Dismissed)),
    }))
}

/// Read where an inline comment anchors, falling back to the position it was
/// first recorded at when its current line no longer exists in the diff. GitHub
/// can place the two ends of a multi-line comment on different sides of the
/// diff; a range that crosses sides is anchored to its end line's side alone,
/// since a single-side range cannot span both.
fn anchor_of(comment: &octocrab::models::pulls::Comment) -> Result<ForgeAnchor> {
    let (end, start, commit) = match comment.line {
        Some(line) => (line, comment.start_line, comment.commit_id.clone()),
        None => (
            comment
                .original_line
                .context("a github review comment has neither a current nor an original line")?,
            comment.original_start_line,
            comment.original_commit_id.clone(),
        ),
    };
    // `side` names the end line's side and `start_side`, present only on a
    // multi-line comment, the first line's. GitHub keeps no separate side for an
    // outdated comment, so these current sides describe the original position
    // too.
    let end_side = side_of(comment.side.as_deref());
    let start_side = match comment.start_side.as_deref() {
        Some(start_side) => side_of(Some(start_side)),
        None => end_side,
    };
    let end_line = line_no(end)?;
    // A range crossing sides has no single-side start, so it collapses onto the
    // end line; on a shared side the start line bounds the range.
    let start_line = match start {
        Some(start) if start_side == end_side => line_no(start)?,
        _ => end_line,
    };
    if start_line > end_line {
        bail!("a github review comment starts below where it ends");
    }
    Ok(ForgeAnchor {
        path: comment.path.clone(),
        side: end_side,
        start_line,
        end_line,
        commit: RevisionId(commit),
    })
}

/// Map GitHub's diff side to wiff's. GitHub names the deletion side `LEFT` and
/// defaults an unspecified side to the addition side.
fn side_of(side: Option<&str>) -> Side {
    match side {
        Some("LEFT") => Side::Before,
        _ => Side::After,
    }
}

/// Wrap a 1-based line number, rejecting the zero GitHub never sends for a
/// placed comment.
fn line_no(n: u64) -> Result<LineNo> {
    let n = u32::try_from(n).ok().and_then(LineNo::new);
    n.context("a github review comment has an out-of-range line number")
}

/// Map a GitHub account to a wiff author, taking the login as the name and
/// marking a bot account as an agent. GitHub reports a null account for a
/// comment or review whose author later deleted their account; that maps to a
/// human named "ghost", matching GitHub's own placeholder, rather than failing
/// the whole fetch over one departed author.
fn author_of(user: Option<&octocrab::models::Author>) -> Author {
    let Some(user) = user else {
        return Author {
            name: "ghost".to_string(),
            kind: AuthorKind::Human,
        };
    };
    let kind = if user.r#type.eq_ignore_ascii_case("bot") {
        AuthorKind::Agent
    } else {
        AuthorKind::Human
    };
    Author {
        name: user.login.clone(),
        kind,
    }
}

/// Map a GraphQL actor to a wiff author, marking a `Bot` resolver as an agent
/// and every other actor kind as a human, matching how `author_of` reads a
/// REST account.
fn actor_author(actor: graphql::Actor) -> Author {
    let kind = if actor.typename.eq_ignore_ascii_case("bot") {
        AuthorKind::Agent
    } else {
        AuthorKind::Human
    };
    Author {
        name: actor.login,
        kind,
    }
}

/// Name a forge object of `kind` by its numeric `id`, keeping its web URL for
/// presentation.
fn object_ref(forge: &ForgeId, kind: ExternalKind, id: String, url: Option<String>) -> ExternalRef {
    ExternalRef {
        forge: forge.clone(),
        kind,
        id,
        url,
    }
}

/// Convert a Unix timestamp split into whole seconds and sub-second nanos, as
/// octocrab's `chrono` timestamps expose, into a `time` value.
fn to_time(unix_seconds: i64, subsec_nanos: u32) -> Result<OffsetDateTime> {
    let seconds = OffsetDateTime::from_unix_timestamp(unix_seconds)
        .context("a github timestamp is out of range")?;
    Ok(seconds + Duration::nanoseconds(i64::from(subsec_nanos)))
}

/// Attach each thread's resolution to the fetched comment that roots it, matched
/// by database id. Only inline comments sit in review threads, so review-level
/// comments are left untouched.
fn apply_resolutions(comments: &mut [FetchedComment], resolutions: &HashMap<u64, Resolution>) {
    for comment in comments {
        if comment.anchor.is_none() {
            continue;
        }
        if let Ok(id) = comment.origin.id.parse::<u64>()
            && let Some(resolution) = resolutions.get(&id)
        {
            comment.resolution = Some(resolution.clone());
        }
    }
}

/// Read a page of review threads, each with its resolution state, its resolver,
/// and the database ids of a first page of its comments. The root comment is
/// the first, keying a resolved thread to the REST comment it shows on; the
/// whole comment page lets an outbound resolve locate a thread from any comment
/// it holds.
const REVIEW_THREADS_QUERY: &str = "\
query ($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          resolvedBy { login __typename }
          comments(first: 100) {
            pageInfo { hasNextPage endCursor }
            nodes { databaseId }
          }
        }
      }
    }
  }
}";

/// Read a further page of one review thread's comments, addressing the thread by
/// its node id, for a thread whose comments overflow the first page.
const THREAD_COMMENTS_QUERY: &str = "\
query ($threadId: ID!, $cursor: String) {
  node(id: $threadId) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { databaseId }
      }
    }
  }
}";

/// Mark a review thread resolved, addressing it by its node id.
const RESOLVE_THREAD_MUTATION: &str = "\
mutation ($threadId: ID!) {
  resolveReviewThread(input: { threadId: $threadId }) { thread { id } }
}";

/// Reopen a resolved review thread, addressing it by its node id.
const UNRESOLVE_THREAD_MUTATION: &str = "\
mutation ($threadId: ID!) {
  unresolveReviewThread(input: { threadId: $threadId }) { thread { id } }
}";

/// The slice of GitHub's GraphQL schema this adapter reads for thread
/// resolution, which octocrab does not type. These shapes deserialize the
/// `data` object octocrab returns from a `reviewThreads` query.
mod graphql {
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ThreadsData {
        pub repository: Repository,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Repository {
        pub pull_request: PullRequest,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PullRequest {
        pub review_threads: ThreadConnection,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ThreadConnection {
        pub page_info: PageInfo,
        pub nodes: Vec<ReviewThread>,
    }

    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    pub struct PageInfo {
        pub has_next_page: bool,
        pub end_cursor: Option<String>,
    }

    impl PageInfo {
        /// The cursor to read the next page from, or `None` when this is the
        /// last page even if a stale `end_cursor` remains.
        pub fn next_cursor(&self) -> Option<String> {
            self.has_next_page
                .then(|| self.end_cursor.clone())
                .flatten()
        }
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ReviewThread {
        pub id: String,
        pub is_resolved: bool,
        pub resolved_by: Option<Actor>,
        pub comments: CommentConnection,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CommentConnection {
        #[serde(default)]
        pub page_info: PageInfo,
        pub nodes: Vec<ThreadComment>,
    }

    impl CommentConnection {
        /// Whether any comment on this page has the database id `comment_id`.
        pub fn holds(&self, comment_id: u64) -> bool {
            self.nodes
                .iter()
                .any(|comment| comment.database_id == Some(comment_id))
        }
    }

    /// The `data` object a `node(id:)` query returns when the node is a review
    /// thread, deserializing the inline fragment's `comments` selection.
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ThreadNodeData {
        pub node: ThreadNode,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ThreadNode {
        pub comments: CommentConnection,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ThreadComment {
        pub database_id: Option<u64>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Actor {
        pub login: String,
        /// GitHub's GraphQL type name for the resolver, `Bot` for a GitHub App
        /// and `User` for a person. Defaulted for a response that omits it.
        #[serde(default, rename = "__typename")]
        pub typename: String,
    }
}
