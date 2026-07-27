//! A fake [`Forge`] confirms the trait is implementable and the neutral types
//! travel through it, so the shape is settled before an adapter depends on it.

use std::collections::BTreeMap;

use anyhow::Result;
use async_trait::async_trait;
use time::OffsetDateTime;
use ulid::Ulid;
use wiff_core::record::{
    Author, AuthorKind, Description, Disposition, ExternalKind, ExternalRef, ForgeId, ForgeUrl,
    RevisionId,
};
use wiff_core::source::FetchSource;
use wiff_diff::{LineNo, Side};
use wiff_forge::{
    ChangedFile, FetchedComment, FetchedDescription, FetchedPullRequest, FetchedReview, Forge,
    ForgeAnchor, NewPullRequest, OutgoingComment, OutgoingReview, Resolution, SubmittedReview,
    Unsupported,
};

fn forge_id() -> ForgeId {
    ForgeId {
        provider: "github".to_string(),
        host: "github.com".to_string(),
    }
}

fn external_ref(id: &str) -> ExternalRef {
    ExternalRef {
        forge: forge_id(),
        kind: ExternalKind::ReviewComment,
        id: id.to_string(),
        url: Some(format!("https://github.com/o/r/pull/1#{id}")),
    }
}

fn line(n: u32) -> LineNo {
    LineNo::new(n).expect("nonzero line")
}

fn fetched_description() -> FetchedDescription {
    FetchedDescription {
        origin: ExternalRef {
            forge: forge_id(),
            kind: ExternalKind::Description,
            id: "1".to_string(),
            url: Some("https://github.com/o/r/pull/1".to_string()),
        },
        author: Author {
            name: "octocat".to_string(),
            kind: AuthorKind::Human,
        },
        content: Description {
            title: "Refactor the widget".to_string(),
            body: "Splits the widget in two.".to_string(),
        },
        authored_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp"),
    }
}

/// A canned forge that echoes the pull request URL it was asked about and
/// returns fixed comments and reviews, so a caller can drive the trait without
/// a network.
struct FakeForge;

#[async_trait]
impl Forge for FakeForge {
    async fn fetch(&self, pr: &ForgeUrl) -> Result<FetchedPullRequest> {
        Ok(FetchedPullRequest {
            url: pr.clone(),
            description: fetched_description(),
            head: FetchSource::Git {
                url: "https://github.com/o/r".to_string(),
                git_ref: "refs/pull/1/head".to_string(),
                commit: RevisionId("headcommit".to_string()),
            },
            base: FetchSource::Git {
                url: "https://github.com/o/r".to_string(),
                git_ref: "refs/heads/main".to_string(),
                commit: RevisionId("basecommit".to_string()),
            },
            comments: vec![FetchedComment {
                origin: external_ref("c1"),
                author: Author {
                    name: "octocat".to_string(),
                    kind: AuthorKind::Human,
                },
                body: "needs a test".to_string(),
                authored_at: OffsetDateTime::from_unix_timestamp(1_700_000_000)
                    .expect("valid timestamp"),
                anchor: Some(ForgeAnchor {
                    path: "src/widget.rs".to_string(),
                    side: Side::After,
                    start_line: line(10),
                    end_line: line(12),
                    commit: RevisionId("headcommit".to_string()),
                }),
                reply_to: None,
                resolution: Some(Resolution { by: None }),
            }],
            reviews: vec![FetchedReview {
                origin: external_ref("r1"),
                author: Author {
                    name: "reviewer".to_string(),
                    kind: AuthorKind::Human,
                },
                body: "looks good".to_string(),
                authored_at: OffsetDateTime::from_unix_timestamp(1_700_000_100)
                    .expect("valid timestamp"),
                disposition: Some(Disposition::Approve),
                dismissed: false,
            }],
        })
    }

    async fn fetch_changed_files(&self, _pr: &ForgeUrl) -> Result<Vec<ChangedFile>> {
        unreachable!("this fake serves no changed files")
    }

    async fn submit_review(
        &self,
        _pr: &ForgeUrl,
        review: &OutgoingReview,
    ) -> Result<SubmittedReview> {
        let comments = review
            .comments
            .iter()
            .map(|comment| (comment.comment, external_ref(&comment.comment.to_string())))
            .collect();
        Ok(SubmittedReview {
            review: external_ref("review"),
            comments,
        })
    }

    async fn post_comment(&self, _pr: &ForgeUrl, comment: &OutgoingComment) -> Result<ExternalRef> {
        Ok(external_ref(&comment.comment.to_string()))
    }

    async fn edit_comment(&self, _at: &ExternalRef, _body: &str) -> Result<()> {
        Ok(())
    }

    async fn set_resolved(&self, _at: &ExternalRef, _resolved: bool) -> Result<()> {
        Err(Unsupported.into())
    }

    async fn set_description(&self, _pr: &ForgeUrl, _description: &Description) -> Result<()> {
        Ok(())
    }

    async fn create_pull_request(&self, _req: &NewPullRequest) -> Result<ForgeUrl> {
        ForgeUrl::parse("https://github.com/o/r/pull/2").map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn pull_request_url(&self, _remote_url: &str, _id: &str) -> Result<ForgeUrl> {
        unreachable!("this fake resolves no pull request by id")
    }

    fn project_bucket(&self, _pr: &ForgeUrl) -> Result<String> {
        unreachable!("this fake names no project bucket")
    }

    fn matches_remote(&self, _pr: &ForgeUrl, _remote_url: &str) -> Result<bool> {
        unreachable!("this fake matches no remote")
    }
}

#[tokio::test]
async fn fetch_returns_the_pull_request_the_forge_reports() {
    let forge = FakeForge;
    let url = ForgeUrl::parse("https://github.com/o/r/pull/1").expect("valid url");

    let fetched = forge.fetch(&url).await.expect("fetch succeeds");

    wince::assert_eq!(
        fetched,
        FetchedPullRequest {
            url: ForgeUrl::parse("https://github.com/o/r/pull/1").expect("valid url"),
            description: fetched_description(),
            head: FetchSource::Git {
                url: "https://github.com/o/r".to_string(),
                git_ref: "refs/pull/1/head".to_string(),
                commit: RevisionId("headcommit".to_string()),
            },
            base: FetchSource::Git {
                url: "https://github.com/o/r".to_string(),
                git_ref: "refs/heads/main".to_string(),
                commit: RevisionId("basecommit".to_string()),
            },
            comments: vec![FetchedComment {
                origin: external_ref("c1"),
                author: Author {
                    name: "octocat".to_string(),
                    kind: AuthorKind::Human,
                },
                body: "needs a test".to_string(),
                authored_at: OffsetDateTime::from_unix_timestamp(1_700_000_000)
                    .expect("valid timestamp"),
                anchor: Some(ForgeAnchor {
                    path: "src/widget.rs".to_string(),
                    side: Side::After,
                    start_line: line(10),
                    end_line: line(12),
                    commit: RevisionId("headcommit".to_string()),
                }),
                reply_to: None,
                resolution: Some(Resolution { by: None }),
            }],
            reviews: vec![FetchedReview {
                origin: external_ref("r1"),
                author: Author {
                    name: "reviewer".to_string(),
                    kind: AuthorKind::Human,
                },
                body: "looks good".to_string(),
                authored_at: OffsetDateTime::from_unix_timestamp(1_700_000_100)
                    .expect("valid timestamp"),
                disposition: Some(Disposition::Approve),
                dismissed: false,
            }],
        }
    );
}

#[tokio::test]
async fn submit_review_keys_created_objects_to_their_local_comments() {
    let forge = FakeForge;
    let url = ForgeUrl::parse("https://github.com/o/r/pull/1").expect("valid url");
    let comment = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let review = OutgoingReview {
        disposition: Some(Disposition::RequestChanges),
        body: "one blocker".to_string(),
        comments: vec![OutgoingComment {
            comment,
            body: "fix this".to_string(),
            disposition: None,
            anchor: None,
            reply_to: None,
        }],
    };

    let submitted = forge
        .submit_review(&url, &review)
        .await
        .expect("submit succeeds");

    wince::assert_eq!(
        submitted,
        SubmittedReview {
            review: external_ref("review"),
            comments: BTreeMap::from([(comment, external_ref("01ARZ3NDEKTSV4RRFFQ69G5FAV"))]),
        }
    );
}

#[tokio::test]
async fn an_unsupported_operation_reports_itself() {
    let forge = FakeForge;
    let at = external_ref("c1");

    let error = forge
        .set_resolved(&at, true)
        .await
        .expect_err("the fake cannot resolve");

    wince::assert_eq!(error.downcast_ref::<Unsupported>(), Some(&Unsupported));
}
