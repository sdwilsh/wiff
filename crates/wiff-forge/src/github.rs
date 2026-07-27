//! The GitHub forge adapter, built on `octocrab`. It translates octocrab's
//! typed REST models into wiff's neutral shapes and converts their `chrono`
//! timestamps to `time`, keeping every GitHub-specific type within this module.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use futures::stream::{StreamExt as _, TryStreamExt as _};
use octocrab::Octocrab;
use octocrab::models::ReviewId;
use octocrab::models::repos::DiffEntryStatus;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tracing::warn;
use url::Url;
use wiff_core::record::{
    Author, AuthorKind, Description, Disposition, ExternalKind, ExternalRef, ForgeId, ForgeUrl,
    RevisionId,
};
use wiff_core::source::FetchSource;
use wiff_diff::{FileStatus, LineNo, Side};

use crate::blob_diff::{ChangedFile, Content};
use crate::clone_url::parse_clone_url;
use crate::types::{
    FetchedComment, FetchedDescription, FetchedPullRequest, FetchedReview, ForgeAnchor,
    NewPullRequest, OutgoingComment, OutgoingReview, Resolution, SubmittedReview,
};

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
        let base = base_source(pr, &at, &meta);
        let description = fetched_description(&at, &forge, &meta)?;

        Ok(FetchedPullRequest {
            url: pr.clone(),
            description,
            head,
            base,
            comments,
            reviews,
        })
    }

    /// Fetch every changed file of the pull request with the base and head
    /// contents needed to assemble its diff without a local clone. A file too
    /// large to fetch, or whose content is not valid UTF-8, is reported as
    /// binary rather than diffed.
    pub async fn fetch_changed_files(&self, pr: &ForgeUrl) -> Result<Vec<ChangedFile>> {
        let at = PullRequestId::parse(pr)?;
        let meta = self
            .crab
            .pulls(&at.owner, &at.repo)
            .get(at.number)
            .await
            .with_context(|| format!("fetching {pr}"))?;
        let base_sha = meta.base.sha.clone();
        let head_sha = meta.head.sha.clone();

        let first = self
            .crab
            .pulls(&at.owner, &at.repo)
            .list_files(at.number)
            .await
            .with_context(|| format!("listing the changed files of {pr}"))?;
        let entries = self
            .crab
            .all_pages(first)
            .await
            .with_context(|| format!("listing the changed files of {pr}"))?;

        let plans = entries
            .into_iter()
            .map(|entry| {
                FilePlan::from_entry(
                    entry.status,
                    &entry.filename,
                    entry.previous_filename.as_deref(),
                )
            })
            .collect::<Result<Vec<_>>>()?;

        // Fetch the files concurrently, bounded so a wide pull request does not
        // open one request per file simultaneously, while `buffered` keeps the
        // results in the listing's order for a stable assembled diff.
        let files = futures::stream::iter(plans)
            .map(|plan| self.fetch_file(&at, &base_sha, &head_sha, plan))
            .buffered(FETCH_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(files)
    }

    /// Fetch the sides `plan` calls for and assemble them into one changed file.
    async fn fetch_file(
        &self,
        at: &PullRequestId,
        base_sha: &str,
        head_sha: &str,
        plan: FilePlan,
    ) -> Result<ChangedFile> {
        let before = match plan.fetch_base {
            true => Some(self.fetch_side(at, &plan.old_path, base_sha).await?),
            false => None,
        };
        let after = match plan.fetch_head {
            true => Some(self.fetch_side(at, &plan.new_path, head_sha).await?),
            false => None,
        };
        Ok(ChangedFile {
            status: plan.status,
            old_path: plan.old_path,
            new_path: plan.new_path,
            content: content_of(before, after),
        })
    }

    /// Fetch one side's blob content for `path` at `commit`, reading the file
    /// through the contents API and falling back to the blobs API for a file
    /// larger than the contents API serves inline. A file past the API's own
    /// ceiling, which the contents API refuses to serve, is reported as too
    /// large rather than fetched.
    async fn fetch_side(&self, at: &PullRequestId, path: &str, commit: &str) -> Result<SideBlob> {
        let route = format!(
            "/repos/{}/{}/contents/{}",
            at.owner,
            at.repo,
            utf8_percent_encode(path, CONTENTS_PATH)
        );
        let content: ContentResponse = match self.crab.get(&route, Some(&[("ref", commit)])).await {
            Ok(content) => content,
            // A file past the API's size ceiling is refused with a 403 whose
            // error code says so, rather than a body wiff could read a size
            // from; that refusal renders the file binary.
            Err(err) if is_too_large(&err) => return Ok(SideBlob::TooLarge),
            Err(err) => return Err(err).with_context(|| format!("fetching {path} at {commit}")),
        };
        // Above the contents API's inline size, the response omits the content
        // and marks the encoding "none"; the blobs API serves the full bytes by
        // sha up to a far larger ceiling.
        let bytes = match content.content {
            Some(encoded) if content.encoding.as_deref() == Some("base64") => {
                decode_base64(&encoded)?
            }
            _ => self.fetch_blob(at, &content.sha).await?,
        };
        Ok(SideBlob::Bytes(bytes))
    }

    /// Fetch a blob's bytes by its `sha` through the git blobs API.
    async fn fetch_blob(&self, at: &PullRequestId, sha: &str) -> Result<Vec<u8>> {
        let route = format!("/repos/{}/{}/git/blobs/{}", at.owner, at.repo, sha);
        let blob: BlobResponse = self
            .crab
            .get(&route, None::<&()>)
            .await
            .with_context(|| format!("fetching blob {sha}"))?;
        if blob.encoding != "base64" {
            bail!(
                "blob {sha} came back in unexpected encoding {}",
                blob.encoding
            );
        }
        decode_base64(&blob.content)
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

    /// Submit a batched review as one call: the summary and verdict together
    /// with the fresh anchored comments, which either post whole or not at all.
    /// Replies and review-level fallbacks are not part of the batch; the caller
    /// posts those through [`post_comment`](Self::post_comment). GitHub returns
    /// only the review object, so its comments are read back and matched to their
    /// local counterparts by anchor.
    ///
    /// The two calls are not transactional: once the review POST succeeds the
    /// review exists on the forge, so a read-back or matching failure names the
    /// created review in its error, letting the caller reconcile rather than
    /// re-submit a duplicate.
    pub async fn submit_review(
        &self,
        pr: &ForgeUrl,
        review: &OutgoingReview,
    ) -> Result<SubmittedReview> {
        let at = PullRequestId::parse(pr)?;
        let forge = ForgeId {
            provider: "github".to_string(),
            host: pr.host(),
        };
        let comments = review
            .comments
            .iter()
            .map(review_comment_payload)
            .collect::<Result<Vec<_>>>()?;
        let body = json!({
            "body": review.body,
            "event": review_event(review.disposition),
            "comments": comments,
        });
        let route = format!(
            "/repos/{}/{}/pulls/{}/reviews",
            at.owner, at.repo, at.number
        );
        let created: CreatedObject = self
            .crab
            .post(route, Some(&body))
            .await
            .with_context(|| format!("submitting a review on {pr}"))?;

        let mut placed = self
            .placed_review_comments(&at, ReviewId::from(created.id))
            .await?;
        if placed.len() != review.comments.len() {
            bail!(
                "github created review {} ({}) with {} comments for {} submitted; \
                 the review exists on the forge and must be reconciled, not re-submitted",
                created.id,
                created.html_url,
                placed.len(),
                review.comments.len()
            );
        }
        let comments = review
            .comments
            .iter()
            .map(|outgoing| {
                let matched = match_placed(&placed, outgoing).with_context(|| {
                    format!(
                        "binding a comment of review {} ({}) failed; the review exists \
                         on the forge and must be reconciled, not re-submitted",
                        created.id, created.html_url
                    )
                })?;
                let made = placed.swap_remove(matched);
                Ok((outgoing.comment, created_comment_ref(&forge, made)))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(SubmittedReview {
            review: object_ref(
                &forge,
                ExternalKind::Verdict,
                created.id.to_string(),
                Some(created.html_url),
            ),
            comments,
        })
    }

    /// Post a standalone comment: a reply to an existing thread, a fresh inline
    /// comment outside a batched review, or a review-level comment that anchors
    /// nowhere on the diff. Which endpoint is used follows from whether the
    /// comment replies, anchors inline, or does neither.
    pub async fn post_comment(
        &self,
        pr: &ForgeUrl,
        comment: &OutgoingComment,
    ) -> Result<ExternalRef> {
        let at = PullRequestId::parse(pr)?;
        let forge = ForgeId {
            provider: "github".to_string(),
            host: pr.host(),
        };
        let text = comment_body(comment);
        let created: CreatedObject = if let Some(reply_to) = &comment.reply_to {
            let parent: u64 = reply_to
                .id
                .parse()
                .with_context(|| format!("{} is not a github comment id", reply_to.id))?;
            let route = format!(
                "/repos/{}/{}/pulls/{}/comments",
                at.owner, at.repo, at.number
            );
            self.crab
                .post(route, Some(&json!({ "body": text, "in_reply_to": parent })))
                .await
                .with_context(|| format!("replying to comment {parent} on {pr}"))?
        } else if let Some(anchor) = &comment.anchor {
            let mut payload = anchor_payload(anchor, &text);
            payload["commit_id"] = json!(anchor.commit.0);
            let route = format!(
                "/repos/{}/{}/pulls/{}/comments",
                at.owner, at.repo, at.number
            );
            self.crab
                .post(route, Some(&payload))
                .await
                .with_context(|| format!("posting an inline comment on {pr}"))?
        } else {
            let route = format!(
                "/repos/{}/{}/issues/{}/comments",
                at.owner, at.repo, at.number
            );
            self.crab
                .post(route, Some(&json!({ "body": text })))
                .await
                .with_context(|| format!("posting a review-level comment on {pr}"))?
        };
        Ok(object_ref(
            &forge,
            ExternalKind::ReviewComment,
            created.id.to_string(),
            Some(created.html_url),
        ))
    }

    /// Re-publish the body of an object already linked to the forge. An inline
    /// review comment, a review-level issue comment, and a review's own summary
    /// body edit through different endpoints and methods, told apart by the
    /// object's web URL.
    pub async fn edit_comment(&self, at: &ExternalRef, body: &str) -> Result<()> {
        let location = CommentLocation::parse(at)?;
        let id: u64 = at
            .id
            .parse()
            .with_context(|| format!("{} is not a github comment id", at.id))?;
        let (method, route) = location.edit_route(id);
        let payload = json!({ "body": body });
        let _: IgnoredAny = match method {
            EditMethod::Patch => self.crab.patch(route, Some(&payload)).await,
            EditMethod::Put => self.crab.put(route, Some(&payload)).await,
        }
        .with_context(|| format!("editing comment {id}"))?;
        Ok(())
    }

    /// Update the pull request's title and body outside a batched review.
    pub async fn set_description(&self, pr: &ForgeUrl, description: &Description) -> Result<()> {
        let at = PullRequestId::parse(pr)?;
        let route = format!("/repos/{}/{}/pulls/{}", at.owner, at.repo, at.number);
        let _: IgnoredAny = self
            .crab
            .patch(
                route,
                Some(&json!({ "title": description.title, "body": description.body })),
            )
            .await
            .with_context(|| format!("updating the description of {pr}"))?;
        Ok(())
    }

    /// Open a pull request from an already-pushed branch, returning its URL.
    pub async fn create_pull_request(&self, req: &NewPullRequest) -> Result<ForgeUrl> {
        let at = RepoId::parse(&req.repo)?;
        let route = format!("/repos/{}/{}/pulls", at.owner, at.repo);
        let body = json!({
            "title": req.description.title,
            "body": req.description.body,
            "head": req.head_branch,
            "base": req.base_branch,
        });
        let created: CreatedObject = self
            .crab
            .post(route, Some(&body))
            .await
            .with_context(|| format!("opening a pull request in {}", req.repo))?;
        ForgeUrl::parse(&created.html_url)
            .with_context(|| format!("{} is not a pull request URL", created.html_url))
    }

    /// Read the inline comments GitHub created for review `review_id`, resolved
    /// to the anchor each sits on. The comments are read through the pull
    /// request's comment listing rather than the review's own, since only that
    /// listing reports a comment's line and side: the review-scoped listing
    /// gives just a diff-offset position, which cannot key a comment back to the
    /// local one it was submitted for. The listing order is GitHub's own; the
    /// caller matches each back to a submitted comment by anchor and body.
    async fn placed_review_comments(
        &self,
        at: &PullRequestId,
        review_id: ReviewId,
    ) -> Result<Vec<PlacedComment>> {
        let first = self
            .crab
            .pulls(&at.owner, &at.repo)
            .list_comments(Some(at.number))
            .send()
            .await
            .context("listing a submitted review's comments")?;
        let all = self
            .crab
            .all_pages(first)
            .await
            .context("listing a submitted review's comments")?;
        all.into_iter()
            .filter(|comment| comment.pull_request_review_id == Some(review_id))
            .map(PlacedComment::from_comment)
            .collect()
    }
}

#[async_trait::async_trait]
impl crate::Forge for GithubForge {
    async fn fetch(&self, pr: &ForgeUrl) -> Result<FetchedPullRequest> {
        self.fetch(pr).await
    }

    async fn fetch_changed_files(&self, pr: &ForgeUrl) -> Result<Vec<ChangedFile>> {
        self.fetch_changed_files(pr).await
    }

    fn pull_request_url(&self, remote_url: &str, id: &str) -> Result<ForgeUrl> {
        let (mut url, owner, repo) = parse_remote(remote_url)?;
        // Extend the path through url::Url so an id holding a URL-reserved
        // character (the caller treats it as opaque) is percent-encoded rather
        // than corrupting the path.
        url.path_segments_mut()
            .expect("a scheme/host web base is always a base URL")
            .extend([owner.as_str(), repo.as_str(), "pull", id]);
        Ok(ForgeUrl::parse(url.as_str())?)
    }

    fn project_bucket(&self, pr: &ForgeUrl) -> Result<String> {
        let at = PullRequestId::parse(pr)?;
        // GitHub addresses an owner and repository case-insensitively, so lower
        // them to match the casing the host itself ignores; the same PR pulled
        // under differently-cased URLs then keys to one bucket.
        Ok(format!(
            "{}/{}/{}",
            pr.host(),
            at.owner.to_ascii_lowercase(),
            at.repo.to_ascii_lowercase()
        ))
    }

    fn matches_remote(&self, pr: &ForgeUrl, remote_url: &str) -> Result<bool> {
        let at = PullRequestId::parse(pr)?;
        // A remote this adapter cannot read as a github repository (a local path
        // or another forge's host) simply does not address this pull request.
        let Ok((web, owner, repo)) = parse_remote(remote_url) else {
            return Ok(false);
        };
        Ok(web.host_str() == Some(pr.host().as_str())
            && owner.eq_ignore_ascii_case(&at.owner)
            && repo.eq_ignore_ascii_case(&at.repo))
    }

    async fn submit_review(
        &self,
        pr: &ForgeUrl,
        review: &OutgoingReview,
    ) -> Result<SubmittedReview> {
        self.submit_review(pr, review).await
    }

    async fn post_comment(&self, pr: &ForgeUrl, comment: &OutgoingComment) -> Result<ExternalRef> {
        self.post_comment(pr, comment).await
    }

    async fn edit_comment(&self, at: &ExternalRef, body: &str) -> Result<()> {
        self.edit_comment(at, body).await
    }

    async fn set_resolved(&self, at: &ExternalRef, resolved: bool) -> Result<()> {
        self.set_resolved(at, resolved).await
    }

    async fn set_description(&self, pr: &ForgeUrl, description: &Description) -> Result<()> {
        self.set_description(pr, description).await
    }

    async fn create_pull_request(&self, req: &NewPullRequest) -> Result<ForgeUrl> {
        self.create_pull_request(req).await
    }
}

/// How many changed files wiff fetches concurrently. A wide pull request runs
/// its per-file fetches in parallel up to this bound, kept modest to stay
/// within GitHub's rate and abuse limits.
const FETCH_CONCURRENCY: usize = 8;

/// The characters escaped when a file path is placed into a contents API route.
/// The set covers the reserved and unsafe bytes a path may hold while leaving
/// the `/` separators intact.
const CONTENTS_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// The metadata the contents API returns for a file, with its content inline
/// for a file small enough to serve that way.
#[derive(Deserialize)]
struct ContentResponse {
    sha: String,
    encoding: Option<String>,
    content: Option<String>,
}

/// Whether an octocrab error is GitHub refusing to serve a file past its size
/// ceiling: a 403 whose `too_large` error code says so. A plain 403 for another
/// reason (a rate limit, say) is not this and must propagate.
fn is_too_large(err: &octocrab::Error) -> bool {
    let octocrab::Error::GitHub { source, .. } = err else {
        return false;
    };
    source.status_code.as_u16() == 403
        && source
            .errors
            .iter()
            .flatten()
            .any(|error| error.get("code").and_then(Value::as_str) == Some("too_large"))
}

/// A blob's bytes as the git blobs API returns them.
#[derive(Deserialize)]
struct BlobResponse {
    content: String,
    encoding: String,
}

/// One side's fetched content, or a marker that the file is too large to fetch.
enum SideBlob {
    /// The decoded bytes of the file at a revision.
    Bytes(Vec<u8>),
    /// The file exceeds the size wiff fetches; render it as binary.
    TooLarge,
}

/// How a changed file maps into wiff's diff model: its status, the paths on each
/// side, and which sides have content to fetch.
struct FilePlan {
    status: FileStatus,
    old_path: String,
    new_path: String,
    fetch_base: bool,
    fetch_head: bool,
}

impl FilePlan {
    /// Map a changed file's GitHub status and paths into a plan. A renamed file
    /// needs the path it moved from, which GitHub reports in `previous_filename`.
    fn from_entry(
        status: DiffEntryStatus,
        filename: &str,
        previous_filename: Option<&str>,
    ) -> Result<Self> {
        let name = filename.to_string();
        Ok(match status {
            // A copy adds a new file from another's content; wiff has no copy
            // status, so it reads as an add of the new path.
            DiffEntryStatus::Added | DiffEntryStatus::Copied => Self {
                status: FileStatus::Added,
                old_path: name.clone(),
                new_path: name,
                fetch_base: false,
                fetch_head: true,
            },
            DiffEntryStatus::Removed => Self {
                status: FileStatus::Deleted,
                old_path: name.clone(),
                new_path: name,
                fetch_base: true,
                fetch_head: false,
            },
            DiffEntryStatus::Modified | DiffEntryStatus::Changed => Self {
                status: FileStatus::Modified,
                old_path: name.clone(),
                new_path: name,
                fetch_base: true,
                fetch_head: true,
            },
            DiffEntryStatus::Renamed => Self {
                status: FileStatus::Renamed,
                old_path: previous_filename
                    .ok_or_else(|| anyhow!("renamed file {name} has no previous path"))?
                    .to_string(),
                new_path: name,
                fetch_base: true,
                fetch_head: true,
            },
            DiffEntryStatus::Unchanged => {
                bail!("file {name} is listed as changed but reports no change")
            }
            other => bail!("file {name} has an unrecognized change status {other:?}"),
        })
    }
}

/// Classify a changed file's two fetched sides into diff content, reporting a
/// file binary when either side is too large to fetch.
fn content_of(before: Option<SideBlob>, after: Option<SideBlob>) -> Content {
    if matches!(before, Some(SideBlob::TooLarge)) || matches!(after, Some(SideBlob::TooLarge)) {
        return Content::Binary;
    }
    let bytes = |side: Option<SideBlob>| match side {
        Some(SideBlob::Bytes(bytes)) => Some(bytes),
        _ => None,
    };
    Content::from_sides(bytes(before), bytes(after))
}

/// Decode GitHub's base64, which wraps its output across lines that must be
/// stripped before decoding.
fn decode_base64(encoded: &str) -> Result<Vec<u8>> {
    let mut bytes = encoded.as_bytes().to_vec();
    bytes.retain(|b| !b.is_ascii_whitespace());
    BASE64_STANDARD
        .decode(bytes)
        .context("decoding base64 content")
}

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

/// A repository's coordinates on GitHub, read from the two path segments of its
/// web URL, `/<owner>/<repo>`. A URL with further segments (a pull request URL,
/// say) is rejected rather than truncated to its owner and repo.
struct RepoId {
    owner: String,
    repo: String,
}

impl RepoId {
    /// Read the owner and repository from `repo`.
    fn parse(repo: &ForgeUrl) -> Result<Self> {
        let url = Url::parse(repo.as_str()).with_context(|| format!("parsing {repo}"))?;
        let mut segments = url.path_segments().into_iter().flatten();
        let owner = segments.next().unwrap_or_default();
        let name = segments.next().unwrap_or_default();
        let trailing = segments.next().unwrap_or_default();
        if owner.is_empty() || name.is_empty() || !trailing.is_empty() {
            bail!("{repo} is not a github repository URL");
        }
        Ok(Self {
            owner: owner.to_string(),
            repo: name.to_string(),
        })
    }
}

/// The base repository's clone URL, where both the pull request's head and its
/// target branch are fetched from. GitHub serves the head under
/// `refs/pull/<number>/head` on the base repository, which reaches a fork
/// without adding a remote for it, and the target branch lives there too. When
/// the API omits the `clone_url`, it is reconstructed from the pull request's
/// own scheme and authority.
fn base_repo_clone_url(
    pr: &ForgeUrl,
    at: &PullRequestId,
    meta: &octocrab::models::pulls::PullRequest,
) -> String {
    meta.base
        .repo
        .as_ref()
        .and_then(|repo| repo.clone_url.as_ref())
        .map(Url::to_string)
        .unwrap_or_else(|| fallback_clone_url(pr, at))
}

/// The fetch that brings the pull request's head down, through the base
/// repository's `refs/pull/<number>/head`.
fn head_source(
    pr: &ForgeUrl,
    at: &PullRequestId,
    meta: &octocrab::models::pulls::PullRequest,
) -> FetchSource {
    FetchSource::Git {
        url: base_repo_clone_url(pr, at, meta),
        git_ref: format!("refs/pull/{}/head", meta.number),
        commit: RevisionId(meta.head.sha.clone()),
    }
}

/// The fetch that brings the target-branch tip down, needed locally to compute
/// the review's base and often absent once the target has advanced past the
/// fork point.
fn base_source(
    pr: &ForgeUrl,
    at: &PullRequestId,
    meta: &octocrab::models::pulls::PullRequest,
) -> FetchSource {
    FetchSource::Git {
        url: base_repo_clone_url(pr, at, meta),
        git_ref: format!("refs/heads/{}", meta.base.ref_field),
        commit: RevisionId(meta.base.sha.clone()),
    }
}

/// Read a git remote's clone URL, in either `https://host/owner/repo(.git)` or
/// scp-style `[user@]host:owner/repo(.git)` form, into the web base that serves
/// its repository together with the owner and repository names. An http(s)
/// remote keeps its scheme and port, so a self-hosted instance answering the
/// web on a non-default port stays reachable; any other transport (ssh, git)
/// resolves to https on the bare host, whose transport port is not the web
/// port. A path that is not exactly an owner and a repository (a GitLab-style
/// subgroup, say) is rejected rather than guessed at, since GitHub addresses a
/// repository by those two segments alone.
fn parse_remote(remote_url: &str) -> Result<(Url, String, String)> {
    let url = parse_clone_url(remote_url)
        .with_context(|| format!("{remote_url} is not a git remote URL"))?;
    let host = url
        .host_str()
        .with_context(|| format!("remote {remote_url} has no host"))?;
    let mut segments = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|segment| !segment.is_empty());
    let owner = segments.next().unwrap_or_default().to_string();
    let repo = segments.next().unwrap_or_default();
    let repo = repo.strip_suffix(".git").unwrap_or(repo).to_string();
    if owner.is_empty() || repo.is_empty() || segments.next().is_some() {
        bail!("{remote_url} is not a github repository remote URL");
    }
    let (scheme, authority) = if matches!(url.scheme(), "http" | "https") {
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        (url.scheme(), authority)
    } else {
        // An ssh or git remote names a transport, not a web location, so keep
        // only its host and address the web over https on the default port:
        // `ssh://git@ghe.corp:2222/octo/demo` gives the web base
        // `https://ghe.corp/`.
        ("https", host.to_string())
    };
    let web = Url::parse(&format!("{scheme}://{authority}/"))
        .with_context(|| format!("deriving the web base for remote {remote_url}"))?;
    Ok((web, owner, repo))
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

/// Map a pull request's title and body to the neutral description, bound to the
/// pull request itself as the forge object it mirrors. GitHub exposes no
/// timestamp for the body alone, so `authored_at` approximates it with the pull
/// request's last-activity time (`updated_at`, which any activity bumps),
/// falling back to its creation time.
fn fetched_description(
    at: &PullRequestId,
    forge: &ForgeId,
    meta: &octocrab::models::pulls::PullRequest,
) -> Result<FetchedDescription> {
    let changed_at = meta
        .updated_at
        .or(meta.created_at)
        .context("a github pull request has neither an updated nor a created time")?;
    Ok(FetchedDescription {
        origin: object_ref(
            forge,
            ExternalKind::Description,
            at.number.to_string(),
            meta.html_url.as_ref().map(ToString::to_string),
        ),
        author: author_of(meta.user.as_deref()),
        content: Description {
            title: meta.title.clone().unwrap_or_default(),
            body: meta.body.clone().unwrap_or_default(),
        },
        authored_at: to_time(changed_at.timestamp(), changed_at.timestamp_subsec_nanos())?,
    })
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

/// Name wiff's diff side in GitHub's terms, the reverse of [`side_of`].
fn side_label(side: Side) -> &'static str {
    match side {
        Side::Before => "LEFT",
        Side::After => "RIGHT",
    }
}

/// The review state to submit for a disposition. An absent disposition submits
/// a plain comment review with no verdict.
fn review_event(disposition: Option<Disposition>) -> &'static str {
    match disposition {
        Some(Disposition::Approve) => "APPROVE",
        Some(Disposition::RequestChanges) => "REQUEST_CHANGES",
        None => "COMMENT",
    }
}

/// The pushed body of a comment. GitHub cannot represent a per-comment
/// disposition natively, so one is rendered as a short tag at the top of the
/// body to keep the signal.
fn comment_body(comment: &OutgoingComment) -> String {
    match comment.disposition {
        Some(Disposition::Approve) => format!("**[approve]**\n\n{}", comment.body),
        Some(Disposition::RequestChanges) => format!("**[request changes]**\n\n{}", comment.body),
        None => comment.body.clone(),
    }
}

/// Build the JSON for a comment to submit within a batched review, from its
/// inline anchor. A batched review comment must anchor inline, since GitHub
/// places each on the diff, and must not be a reply, since a review submission
/// cannot express one. The push layer upholds both by routing replies and
/// unanchored comments through [`post_comment`](GithubForge::post_comment); a
/// violation here is an internal error, not a user's doing.
fn review_comment_payload(comment: &OutgoingComment) -> Result<Value> {
    if comment.reply_to.is_some() {
        bail!("a reply cannot be part of a batched review; post it standalone");
    }
    let anchor = comment
        .anchor
        .as_ref()
        .context("a batched review comment must anchor inline")?;
    Ok(anchor_payload(anchor, &comment_body(comment)))
}

/// Build the JSON fields naming where a comment anchors on the diff, together
/// with its body. A single-line anchor names only its end line; a multi-line
/// one adds the start line and side.
fn anchor_payload(anchor: &ForgeAnchor, body: &str) -> Value {
    let mut payload = json!({
        "path": anchor.path,
        "body": body,
        "line": anchor.end_line.get(),
        "side": side_label(anchor.side),
    });
    if anchor.start_line != anchor.end_line {
        payload["start_line"] = json!(anchor.start_line.get());
        payload["start_side"] = json!(side_label(anchor.side));
    }
    payload
}

/// Name a review comment GitHub created, keeping its web URL for presentation.
fn created_comment_ref(forge: &ForgeId, comment: PlacedComment) -> ExternalRef {
    object_ref(
        forge,
        ExternalKind::ReviewComment,
        comment.id.to_string(),
        Some(comment.html_url),
    )
}

/// Find the created comment that matches `outgoing`, returning its index in
/// `placed`. GitHub does not contract the order of a review's comment listing,
/// so a comment is bound to its forge object by its anchor -- path, line range,
/// and side -- rather than by position. The body disambiguates two comments at
/// the same anchor, but only then: GitHub may normalize a stored body (line
/// endings, trailing whitespace), so requiring an exact body match on every
/// comment would fail a submission that in fact succeeded. A missing or
/// ambiguous match is a hard error, since binding the wrong forge object would
/// misdirect every later edit and resolve.
fn match_placed(placed: &[PlacedComment], outgoing: &OutgoingComment) -> Result<usize> {
    let anchor = outgoing
        .anchor
        .as_ref()
        .context("a batched review comment must anchor inline")?;
    let line = anchor.end_line.get();
    let side = side_label(anchor.side);
    let at_anchor: Vec<usize> = placed
        .iter()
        .enumerate()
        .filter(|(_, made)| {
            made.anchor.path == anchor.path
                && made.anchor.start_line == anchor.start_line
                && made.anchor.end_line == anchor.end_line
                && made.anchor.side == anchor.side
        })
        .map(|(index, _)| index)
        .collect();
    match at_anchor.as_slice() {
        [] => bail!(
            "github created no review comment matching {}:{line} on {side}",
            anchor.path
        ),
        [only] => Ok(*only),
        _ => {
            let body = comment_body(outgoing);
            let mut by_body = at_anchor
                .iter()
                .filter(|&&index| placed[index].body == body);
            let first = by_body.next().with_context(|| {
                format!(
                    "github created several review comments at {}:{line} on {side}, \
                     none with a matching body",
                    anchor.path
                )
            })?;
            if by_body.next().is_some() {
                bail!(
                    "github created several review comments at {}:{line} on {side} \
                     with the same body",
                    anchor.path
                );
            }
            Ok(*first)
        }
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

/// A forge object GitHub created or listed, named by its numeric id and web
/// URL.
#[derive(Deserialize)]
struct CreatedObject {
    id: u64,
    html_url: String,
}

/// A review comment read back after a batched submission, resolved to the
/// anchor and body that key it to the local comment it was created for.
struct PlacedComment {
    id: u64,
    html_url: String,
    /// Where the comment sits on the diff, read the same way a fetched comment's
    /// anchor is so that a line GitHub reports only as an original position
    /// still resolves.
    anchor: ForgeAnchor,
    body: String,
}

impl PlacedComment {
    /// Resolve a read-back GitHub comment to the anchor and body it is keyed by.
    fn from_comment(comment: octocrab::models::pulls::Comment) -> Result<Self> {
        let anchor = anchor_of(&comment)?;
        Ok(Self {
            id: comment.id.into_inner(),
            html_url: comment.html_url,
            anchor,
            body: comment.body,
        })
    }
}

/// Where a linked comment lives, used to choose the endpoint that edits it.
struct CommentLocation {
    owner: String,
    repo: String,
    /// Which endpoint edits this object.
    kind: CommentKind,
}

/// Which comment endpoint a linked comment belongs to. GitHub keeps inline
/// review comments, review-level issue comments, and a review's own summary
/// body in separate id spaces reached through separate routes.
enum CommentKind {
    Inline,
    Issue,
    /// A review's summary body, edited through the pull-review endpoint with a
    /// PUT rather than the PATCH the comment endpoints take. Holds the pull
    /// request number that endpoint's path names, which the comment endpoints
    /// do not use.
    Review {
        number: u64,
    },
}

impl CommentLocation {
    /// Read the repository and comment kind from a linked comment's web URL,
    /// whose path names the repository (`/<owner>/<repo>/...`) and whose
    /// fragment distinguishes an inline comment (`#discussion_r...`), an issue
    /// comment (`#issuecomment-...`), and a review body
    /// (`#pullrequestreview-...`). A review body additionally reads the pull
    /// request number from the `/pull/<number>` path its edit endpoint names.
    fn parse(at: &ExternalRef) -> Result<Self> {
        let web_url = at
            .url
            .as_deref()
            .context("a linked comment has no URL to locate its repository")?;
        let url = Url::parse(web_url).with_context(|| format!("parsing {web_url}"))?;
        let mut segments = url.path_segments().into_iter().flatten();
        let owner = segments.next().unwrap_or_default().to_string();
        let repo = segments.next().unwrap_or_default().to_string();
        if owner.is_empty() || repo.is_empty() {
            bail!("{web_url} names no repository");
        }
        let fragment = url.fragment().unwrap_or_default();
        let kind = if fragment.starts_with("discussion_r") {
            CommentKind::Inline
        } else if fragment.starts_with("issuecomment-") {
            CommentKind::Issue
        } else if fragment.starts_with("pullrequestreview-") {
            // Require the literal `pull` segment before reading the number, so a
            // path of an unexpected shape fails here rather than editing the
            // review of whichever number happens to sit in that position.
            if segments.next() != Some("pull") {
                bail!("{web_url} names no pull request");
            }
            let number = segments
                .next()
                .and_then(|segment| segment.parse().ok())
                .with_context(|| format!("{web_url} names no pull request number"))?;
            CommentKind::Review { number }
        } else {
            bail!("{web_url} is not a github comment URL");
        };
        Ok(Self { owner, repo, kind })
    }

    /// The endpoint that edits the object `id` in this repository, paired with
    /// the HTTP method it takes: a comment is patched, a review body is put.
    fn edit_route(&self, id: u64) -> (EditMethod, String) {
        match self.kind {
            CommentKind::Inline => (
                EditMethod::Patch,
                format!("/repos/{}/{}/pulls/comments/{id}", self.owner, self.repo),
            ),
            CommentKind::Issue => (
                EditMethod::Patch,
                format!("/repos/{}/{}/issues/comments/{id}", self.owner, self.repo),
            ),
            CommentKind::Review { number } => (
                EditMethod::Put,
                format!(
                    "/repos/{}/{}/pulls/{number}/reviews/{id}",
                    self.owner, self.repo
                ),
            ),
        }
    }
}

/// The HTTP method a linked object's edit endpoint takes.
enum EditMethod {
    Patch,
    Put,
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

#[cfg(test)]
mod tests {
    use wiff_core::record::ForgeUrl;

    use octocrab::models::repos::DiffEntryStatus;

    use super::{FilePlan, GithubForge};
    use crate::Forge;

    // Building the octocrab client spawns a background service, so constructing
    // the adapter needs a tokio runtime even though the URL build is offline.
    #[tokio::test]
    async fn pull_request_url_builds_from_https_and_scp_remotes() {
        let forge = GithubForge::new("t", None).expect("adapter");
        let expected = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("url");
        // An https remote and its scp-style twin, with and without the .git
        // suffix, all address the same repository.
        for remote in [
            "https://github.com/octo/demo.git",
            "https://github.com/octo/demo",
            "git@github.com:octo/demo.git",
            "git@github.com:octo/demo",
        ] {
            wince::assert_eq!(
                forge.pull_request_url(remote, "7").expect("url"),
                expected.clone()
            );
        }
    }

    #[tokio::test]
    async fn pull_request_url_keeps_an_http_remote_scheme_and_port() {
        let forge = GithubForge::new("t", None).expect("adapter");
        // A self-hosted instance answering the web on a non-default port keeps
        // both scheme and port; an ssh remote to the same host resolves to
        // https on the default port, since ssh's port is not the web port.
        wince::assert_eq!(
            forge
                .pull_request_url("http://ghe.example.com:8080/octo/demo.git", "7")
                .expect("url"),
            ForgeUrl::parse("http://ghe.example.com:8080/octo/demo/pull/7").expect("url")
        );
        wince::assert_eq!(
            forge
                .pull_request_url("ssh://git@ghe.example.com:2222/octo/demo.git", "7")
                .expect("url"),
            ForgeUrl::parse("https://ghe.example.com/octo/demo/pull/7").expect("url")
        );
    }

    #[tokio::test]
    async fn pull_request_url_percent_encodes_a_reserved_id() {
        let forge = GithubForge::new("t", None).expect("adapter");
        // The id is opaque to wiff; a reserved character is encoded into a
        // single path segment rather than splitting or truncating the path.
        wince::assert_eq!(
            forge
                .pull_request_url("https://github.com/octo/demo.git", "a/b c")
                .expect("url"),
            ForgeUrl::parse("https://github.com/octo/demo/pull/a%2Fb%20c").expect("url")
        );
    }

    #[tokio::test]
    async fn pull_request_url_rejects_a_remote_that_is_not_owner_and_repo() {
        let forge = GithubForge::new("t", None).expect("adapter");
        let error = forge
            .pull_request_url("https://gitlab.com/group/sub/demo.git", "7")
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "https://gitlab.com/group/sub/demo.git is not a github repository remote URL"
        );
    }

    #[tokio::test]
    async fn project_bucket_names_the_host_owner_and_repository() {
        let forge = GithubForge::new("t", None).expect("adapter");
        // A differently-cased owner and repo name the same repository on GitHub,
        // so both spellings key to one lowercased bucket.
        let lower = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("url");
        let mixed = ForgeUrl::parse("https://github.com/Octo/Demo/pull/7").expect("url");
        let buckets = format!(
            "{}\n{}",
            forge.project_bucket(&lower).expect("bucket"),
            forge.project_bucket(&mixed).expect("bucket")
        );
        wince::assert_eq!(
            buckets,
            "github.com/octo/demo\ngithub.com/octo/demo".to_string()
        );
    }

    #[tokio::test]
    async fn matches_remote_compares_host_owner_and_repository() {
        let forge = GithubForge::new("t", None).expect("adapter");
        let pr = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("url");
        let cases = [
            // The same repository in each of git's clone-URL spellings, then
            // differently-cased and `.git`-suffixed forms the forge treats as
            // equal, then a different repo, host, and a non-github remote.
            "https://github.com/octo/demo.git",
            "git@github.com:octo/demo.git",
            "https://github.com/Octo/Demo",
            "https://github.com/octo/other.git",
            "https://gitlab.com/octo/demo.git",
            "/srv/git/demo",
        ];
        let report = cases
            .iter()
            .map(|remote| {
                format!(
                    "{remote} -> {}",
                    forge.matches_remote(&pr, remote).expect("match")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        wince::assert_eq!(
            report,
            "https://github.com/octo/demo.git -> true\n\
             git@github.com:octo/demo.git -> true\n\
             https://github.com/Octo/Demo -> true\n\
             https://github.com/octo/other.git -> false\n\
             https://gitlab.com/octo/demo.git -> false\n\
             /srv/git/demo -> false"
                .to_string()
        );
    }

    /// Describe the plan a status and paths map to: the wiff status, the two
    /// paths, and which sides are fetched.
    fn plan(status: DiffEntryStatus, filename: &str, previous: Option<&str>) -> String {
        match FilePlan::from_entry(status, filename, previous) {
            Ok(plan) => format!(
                "{:?} {} -> {} base={} head={}",
                plan.status, plan.old_path, plan.new_path, plan.fetch_base, plan.fetch_head
            ),
            Err(err) => format!("error: {err}"),
        }
    }

    #[test]
    fn maps_each_change_status_to_a_plan() {
        let cases = [
            plan(DiffEntryStatus::Added, "new.rs", None),
            plan(DiffEntryStatus::Copied, "copy.rs", Some("orig.rs")),
            plan(DiffEntryStatus::Removed, "gone.rs", None),
            plan(DiffEntryStatus::Modified, "edit.rs", None),
            plan(DiffEntryStatus::Changed, "mode.rs", None),
            plan(DiffEntryStatus::Renamed, "to.rs", Some("from.rs")),
            plan(DiffEntryStatus::Renamed, "to.rs", None),
            plan(DiffEntryStatus::Unchanged, "same.rs", None),
        ];
        wince::assert_eq!(
            cases.join("\n"),
            "\
Added new.rs -> new.rs base=false head=true
Added copy.rs -> copy.rs base=false head=true
Deleted gone.rs -> gone.rs base=true head=false
Modified edit.rs -> edit.rs base=true head=true
Modified mode.rs -> mode.rs base=true head=true
Renamed from.rs -> to.rs base=true head=true
error: renamed file to.rs has no previous path
error: file same.rs is listed as changed but reports no change"
        );
    }
}
