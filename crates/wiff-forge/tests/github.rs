#![allow(missing_docs)]

use serde_json::{Value, json};
use time::macros::datetime;
use wiff_core::record::{
    Author, AuthorKind, Description, Disposition, ExternalKind, ExternalRef, ForgeId, ForgeUrl,
    RevisionId,
};
use wiff_core::source::FetchSource;
use wiff_diff::{LineNo, Side};
use wiff_forge::{
    FetchedComment, FetchedPullRequest, FetchedReview, ForgeAnchor, GithubForge, Resolution,
};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a GitHub account JSON object with every field octocrab requires to
/// deserialize an author. A fixture varies only the login and account type.
fn account(login: &str, kind: &str) -> Value {
    json!({
        "login": login,
        "id": 1,
        "node_id": "MDQ6VXNlcjE=",
        "avatar_url": "https://avatars.example.invalid/u.png",
        "gravatar_id": "",
        "url": "https://api.example.invalid/users/one",
        "html_url": "https://example.invalid/one",
        "followers_url": "https://api.example.invalid/users/one/followers",
        "following_url": "https://api.example.invalid/users/one/following",
        "gists_url": "https://api.example.invalid/users/one/gists",
        "starred_url": "https://api.example.invalid/users/one/starred",
        "subscriptions_url": "https://api.example.invalid/users/one/subscriptions",
        "organizations_url": "https://api.example.invalid/users/one/orgs",
        "repos_url": "https://api.example.invalid/users/one/repos",
        "events_url": "https://api.example.invalid/users/one/events",
        "received_events_url": "https://api.example.invalid/users/one/received_events",
        "type": kind,
        "site_admin": false,
    })
}

/// The head commit every fixture's pull request resolves to.
const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
/// The base tip every fixture's pull request targets.
const BASE_SHA: &str = "2222222222222222222222222222222222222222";

/// Build a pull request fixture whose base repository names a `clone_url`,
/// leaving each test to supply only the comments and reviews it exercises.
fn base_pull() -> Value {
    json!({
        "id": 1,
        "number": 7,
        "url": "https://api.example.invalid/repos/octo/demo/pulls/7",
        "locked": false,
        "title": "Add the thing",
        "body": "",
        "head": { "ref": "feature", "sha": HEAD_SHA },
        "base": {
            "ref": "main",
            "sha": BASE_SHA,
            "repo": {
                "id": 2,
                "name": "demo",
                "url": "https://api.example.invalid/repos/octo/demo",
                "clone_url": "https://github.com/octo/demo.git",
            },
        },
    })
}

/// Build a review comment fixture with the required fields, merging `extra` over
/// them to place, reply to, or reassign the author of the comment.
fn review_comment_fixture(id: u64, body: &str, author: Value, extra: Value) -> Value {
    let mut comment = json!({
        "url": format!("https://api.example.invalid/c/{id}"),
        "id": id,
        "node_id": format!("c{id}"),
        "diff_hunk": "@@ -40,3 +40,4 @@",
        "path": "src/lib.rs",
        "commit_id": HEAD_SHA,
        "original_commit_id": HEAD_SHA,
        "body": body,
        "created_at": "2021-06-01T12:00:00Z",
        "updated_at": "2021-06-01T12:00:00Z",
        "html_url": format!("https://github.com/octo/demo/pull/7#discussion_r{id}"),
        "_links": {},
        "user": author,
        "line": 42,
        "side": "RIGHT",
    });
    merge(&mut comment, extra);
    comment
}

/// Overlay every key of `over` onto `base`, replacing whatever it held.
fn merge(base: &mut Value, over: Value) {
    let (Value::Object(base), Value::Object(over)) = (base, over) else {
        return;
    };
    for (key, value) in over {
        base.insert(key, value);
    }
}

/// Build the expected pull request that `base_pull` maps to, holding only the
/// `comments` a test asserts and no reviews.
fn expected_shell(url: &ForgeUrl, comments: Vec<FetchedComment>) -> FetchedPullRequest {
    FetchedPullRequest {
        url: url.clone(),
        description: Description {
            title: "Add the thing".to_string(),
            body: String::new(),
        },
        head: FetchSource::Git {
            url: "https://github.com/octo/demo.git".to_string(),
            git_ref: "refs/pull/7/head".to_string(),
            commit: RevisionId(HEAD_SHA.to_string()),
        },
        base_ref: "main".to_string(),
        base_commit: RevisionId(BASE_SHA.to_string()),
        comments,
        reviews: vec![],
    }
}

/// The `ExternalRef` for review comment `id` on `github.com`, with its web URL
/// as the origin of a fetched comment.
fn review_comment_ref(id: u64) -> ExternalRef {
    ExternalRef {
        forge: ForgeId {
            provider: "github".to_string(),
            host: "github.com".to_string(),
        },
        kind: ExternalKind::ReviewComment,
        id: id.to_string(),
        url: Some(format!(
            "https://github.com/octo/demo/pull/7#discussion_r{id}"
        )),
    }
}

/// Wrap review-thread `nodes` in the GraphQL `reviewThreads` response shape,
/// reporting a single page with no further cursor.
fn threads_response(nodes: Value) -> Value {
    json!({
        "data": {
            "repository": {
                "pullRequest": {
                    "reviewThreads": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": nodes,
                    }
                }
            }
        }
    })
}

/// Mount the four REST read endpoints and the GraphQL endpoint `fetch` calls,
/// serving `threads` as the review-thread nodes, and return a `GithubForge`
/// pointed at the mock server.
async fn github_serving_with_threads(
    server: &MockServer,
    pull: Value,
    review_comments: Value,
    issue_comments: Value,
    reviews: Value,
    threads: Value,
) -> GithubForge {
    for (route, body) in [
        ("/repos/octo/demo/pulls/7", pull),
        ("/repos/octo/demo/pulls/7/comments", review_comments),
        ("/repos/octo/demo/issues/7/comments", issue_comments),
        ("/repos/octo/demo/pulls/7/reviews", reviews),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads_response(threads)))
        .mount(server)
        .await;
    GithubForge::new("token", Some(&server.uri())).expect("build the adapter")
}

/// Mount the read endpoints with no review threads, the common case for a test
/// that does not exercise thread resolution.
async fn github_serving(
    server: &MockServer,
    pull: Value,
    review_comments: Value,
    issue_comments: Value,
    reviews: Value,
) -> GithubForge {
    github_serving_with_threads(
        server,
        pull,
        review_comments,
        issue_comments,
        reviews,
        json!([]),
    )
    .await
}

#[tokio::test]
async fn a_pull_request_maps_to_the_neutral_shape() {
    let server = MockServer::start().await;

    let pull = json!({
        "id": 1,
        "number": 7,
        "url": "https://api.example.invalid/repos/octo/demo/pulls/7",
        "locked": false,
        "title": "Add the thing",
        "body": "The body of the pull request.",
        "head": { "ref": "feature", "sha": "1111111111111111111111111111111111111111" },
        "base": {
            "ref": "main",
            "sha": "2222222222222222222222222222222222222222",
            "repo": {
                "id": 2,
                "name": "demo",
                "url": "https://api.example.invalid/repos/octo/demo",
                "clone_url": "https://github.com/octo/demo.git",
            },
        },
    });

    let review_comments = json!([
        {
            "url": "https://api.example.invalid/c/100",
            "id": 100,
            "node_id": "c100",
            "diff_hunk": "@@ -40,3 +40,4 @@",
            "path": "src/lib.rs",
            "commit_id": "1111111111111111111111111111111111111111",
            "original_commit_id": "1111111111111111111111111111111111111111",
            "body": "A single-line note.",
            "created_at": "2021-06-01T12:00:00Z",
            "updated_at": "2021-06-01T12:00:00Z",
            "html_url": "https://github.com/octo/demo/pull/7#discussion_r100",
            "_links": {},
            "user": account("reviewer", "User"),
            "line": 42,
            "side": "RIGHT",
        },
        {
            "url": "https://api.example.invalid/c/101",
            "id": 101,
            "node_id": "c101",
            "diff_hunk": "@@ -10,3 +10,3 @@",
            "path": "src/old.rs",
            "commit_id": "1111111111111111111111111111111111111111",
            "original_commit_id": "1111111111111111111111111111111111111111",
            "body": "A multi-line reply on the deletion side.",
            "created_at": "2021-06-01T12:01:00Z",
            "updated_at": "2021-06-01T12:01:00Z",
            "html_url": "https://github.com/octo/demo/pull/7#discussion_r101",
            "_links": {},
            "user": account("botford", "Bot"),
            "in_reply_to_id": 100,
            "start_line": 10,
            "line": 12,
            "side": "LEFT",
        },
        {
            "url": "https://api.example.invalid/c/102",
            "id": 102,
            "node_id": "c102",
            "diff_hunk": "@@ -5,3 +5,3 @@",
            "path": "src/gone.rs",
            "commit_id": "1111111111111111111111111111111111111111",
            "original_commit_id": "9999999999999999999999999999999999999999",
            "body": "An outdated comment whose line is gone.",
            "created_at": "2021-06-01T12:02:00Z",
            "updated_at": "2021-06-01T12:02:00Z",
            "html_url": "https://github.com/octo/demo/pull/7#discussion_r102",
            "_links": {},
            "user": account("reviewer", "User"),
            "original_line": 5,
            "side": "RIGHT",
        },
    ]);

    let issue_comments = json!([
        {
            "id": 200,
            "node_id": "i200",
            "url": "https://api.example.invalid/i/200",
            "html_url": "https://github.com/octo/demo/pull/7#issuecomment-200",
            "user": account("reviewer", "User"),
            "created_at": "2021-06-01T12:03:00Z",
            "body": "A review-level comment.",
        }
    ]);

    let reviews = json!([
        {
            "id": 300,
            "node_id": "r300",
            "html_url": "https://github.com/octo/demo/pull/7#pullrequestreview-300",
            "user": account("maintainer", "User"),
            "body": "Looks good.",
            "state": "APPROVED",
            "submitted_at": "2021-06-02T09:00:00Z",
        },
        {
            "id": 301,
            "node_id": "r301",
            "html_url": "https://github.com/octo/demo/pull/7#pullrequestreview-301",
            "user": account("botford", "Bot"),
            "body": "",
            "state": "COMMENTED",
            "submitted_at": "2021-06-02T09:01:00Z",
        },
        {
            "id": 302,
            "node_id": "r302",
            "html_url": "https://github.com/octo/demo/pull/7#pullrequestreview-302",
            "user": account("maintainer", "User"),
            "state": "PENDING",
        },
        {
            "id": 303,
            "node_id": "r303",
            "html_url": "https://github.com/octo/demo/pull/7#pullrequestreview-303",
            "user": account("maintainer", "User"),
            "body": "Withdrawn.",
            "state": "DISMISSED",
            "submitted_at": "2021-06-02T09:02:00Z",
        },
    ]);

    let github = github_serving(&server, pull, review_comments, issue_comments, reviews).await;
    let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

    let fetched = github.fetch(&url).await.expect("fetch succeeds");

    let github_forge = ForgeId {
        provider: "github".to_string(),
        host: "github.com".to_string(),
    };
    let review_comment = |id: &str, url: &str| ExternalRef {
        forge: github_forge.clone(),
        kind: ExternalKind::ReviewComment,
        id: id.to_string(),
        url: Some(url.to_string()),
    };
    let human = |name: &str| Author {
        name: name.to_string(),
        kind: AuthorKind::Human,
    };
    let agent = |name: &str| Author {
        name: name.to_string(),
        kind: AuthorKind::Agent,
    };
    let line = |n: u32| LineNo::new(n).expect("nonzero line");

    let expected = FetchedPullRequest {
        url: url.clone(),
        description: Description {
            title: "Add the thing".to_string(),
            body: "The body of the pull request.".to_string(),
        },
        head: FetchSource::Git {
            url: "https://github.com/octo/demo.git".to_string(),
            git_ref: "refs/pull/7/head".to_string(),
            commit: RevisionId("1111111111111111111111111111111111111111".to_string()),
        },
        base_ref: "main".to_string(),
        base_commit: RevisionId("2222222222222222222222222222222222222222".to_string()),
        comments: vec![
            FetchedComment {
                origin: review_comment(
                    "100",
                    "https://github.com/octo/demo/pull/7#discussion_r100",
                ),
                author: human("reviewer"),
                body: "A single-line note.".to_string(),
                authored_at: datetime!(2021-06-01 12:00:00 UTC),
                anchor: Some(ForgeAnchor {
                    path: "src/lib.rs".to_string(),
                    side: Side::After,
                    start_line: line(42),
                    end_line: line(42),
                    commit: RevisionId("1111111111111111111111111111111111111111".to_string()),
                }),
                reply_to: None,
                resolution: None,
            },
            FetchedComment {
                origin: review_comment(
                    "101",
                    "https://github.com/octo/demo/pull/7#discussion_r101",
                ),
                author: agent("botford"),
                body: "A multi-line reply on the deletion side.".to_string(),
                authored_at: datetime!(2021-06-01 12:01:00 UTC),
                anchor: Some(ForgeAnchor {
                    path: "src/old.rs".to_string(),
                    side: Side::Before,
                    start_line: line(10),
                    end_line: line(12),
                    commit: RevisionId("1111111111111111111111111111111111111111".to_string()),
                }),
                reply_to: Some(ExternalRef {
                    forge: github_forge.clone(),
                    kind: ExternalKind::ReviewComment,
                    id: "100".to_string(),
                    url: None,
                }),
                resolution: None,
            },
            FetchedComment {
                origin: review_comment(
                    "102",
                    "https://github.com/octo/demo/pull/7#discussion_r102",
                ),
                author: human("reviewer"),
                body: "An outdated comment whose line is gone.".to_string(),
                authored_at: datetime!(2021-06-01 12:02:00 UTC),
                anchor: Some(ForgeAnchor {
                    path: "src/gone.rs".to_string(),
                    side: Side::After,
                    start_line: line(5),
                    end_line: line(5),
                    commit: RevisionId("9999999999999999999999999999999999999999".to_string()),
                }),
                reply_to: None,
                resolution: None,
            },
            FetchedComment {
                origin: review_comment(
                    "200",
                    "https://github.com/octo/demo/pull/7#issuecomment-200",
                ),
                author: human("reviewer"),
                body: "A review-level comment.".to_string(),
                authored_at: datetime!(2021-06-01 12:03:00 UTC),
                anchor: None,
                reply_to: None,
                resolution: None,
            },
        ],
        reviews: vec![
            FetchedReview {
                origin: ExternalRef {
                    forge: github_forge.clone(),
                    kind: ExternalKind::Verdict,
                    id: "300".to_string(),
                    url: Some(
                        "https://github.com/octo/demo/pull/7#pullrequestreview-300".to_string(),
                    ),
                },
                author: human("maintainer"),
                body: "Looks good.".to_string(),
                authored_at: datetime!(2021-06-02 09:00:00 UTC),
                disposition: Some(Disposition::Approve),
                dismissed: false,
            },
            FetchedReview {
                origin: ExternalRef {
                    forge: github_forge.clone(),
                    kind: ExternalKind::Verdict,
                    id: "301".to_string(),
                    url: Some(
                        "https://github.com/octo/demo/pull/7#pullrequestreview-301".to_string(),
                    ),
                },
                author: agent("botford"),
                body: String::new(),
                authored_at: datetime!(2021-06-02 09:01:00 UTC),
                disposition: None,
                dismissed: false,
            },
            FetchedReview {
                origin: ExternalRef {
                    forge: github_forge.clone(),
                    kind: ExternalKind::Verdict,
                    id: "303".to_string(),
                    url: Some(
                        "https://github.com/octo/demo/pull/7#pullrequestreview-303".to_string(),
                    ),
                },
                author: human("maintainer"),
                body: "Withdrawn.".to_string(),
                authored_at: datetime!(2021-06-02 09:02:00 UTC),
                disposition: None,
                dismissed: true,
            },
        ],
    };

    wince::assert_eq!(fetched, expected);
}

#[tokio::test]
async fn a_pull_request_without_a_base_clone_url_builds_one_from_its_web_url() {
    let server = MockServer::start().await;

    // The base repo omits clone_url, so the fetch source's URL is derived from
    // the pull request's own web URL.
    let pull = json!({
        "id": 1,
        "number": 7,
        "url": "https://api.example.invalid/repos/octo/demo/pulls/7",
        "locked": false,
        "title": "Add the thing",
        "body": "",
        "head": { "ref": "feature", "sha": "1111111111111111111111111111111111111111" },
        "base": { "ref": "main", "sha": "2222222222222222222222222222222222222222" },
    });

    let github = github_serving(&server, pull, json!([]), json!([]), json!([])).await;
    let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

    let fetched = github.fetch(&url).await.expect("fetch succeeds");

    let expected = FetchedPullRequest {
        url: url.clone(),
        description: Description {
            title: "Add the thing".to_string(),
            body: String::new(),
        },
        head: FetchSource::Git {
            url: "https://github.com/octo/demo.git".to_string(),
            git_ref: "refs/pull/7/head".to_string(),
            commit: RevisionId("1111111111111111111111111111111111111111".to_string()),
        },
        base_ref: "main".to_string(),
        base_commit: RevisionId("2222222222222222222222222222222222222222".to_string()),
        comments: vec![],
        reviews: vec![],
    };

    wince::assert_eq!(fetched, expected);
}

#[tokio::test]
async fn a_url_that_is_not_a_pull_request_is_rejected_before_any_request() {
    let github = GithubForge::new("token", None).expect("build the adapter");
    let url = ForgeUrl::parse("https://github.com/octo/demo/issues/7").expect("valid url");

    let error = github
        .fetch(&url)
        .await
        .expect_err("a non-pull URL is rejected");

    wince::assert_eq!(
        error.to_string(),
        "https://github.com/octo/demo/issues/7 is not a github pull request URL".to_string()
    );
}

#[tokio::test]
async fn a_self_hosted_fallback_clone_url_keeps_the_pull_requests_port() {
    let server = MockServer::start().await;

    // The base repo omits clone_url, so the fetch URL is rebuilt from the pull
    // request's own scheme and authority, including its non-default port.
    let pull = json!({
        "id": 1,
        "number": 7,
        "url": "https://api.example.invalid/repos/octo/demo/pulls/7",
        "locked": false,
        "title": "Add the thing",
        "body": "",
        "head": { "ref": "feature", "sha": HEAD_SHA },
        "base": { "ref": "main", "sha": BASE_SHA },
    });

    let github = github_serving(&server, pull, json!([]), json!([]), json!([])).await;
    let url = ForgeUrl::parse("https://git.example.com:8443/octo/demo/pull/7").expect("valid url");

    let fetched = github.fetch(&url).await.expect("fetch succeeds");

    let expected = FetchedPullRequest {
        url: url.clone(),
        description: Description {
            title: "Add the thing".to_string(),
            body: String::new(),
        },
        head: FetchSource::Git {
            url: "https://git.example.com:8443/octo/demo.git".to_string(),
            git_ref: "refs/pull/7/head".to_string(),
            commit: RevisionId(HEAD_SHA.to_string()),
        },
        base_ref: "main".to_string(),
        base_commit: RevisionId(BASE_SHA.to_string()),
        comments: vec![],
        reviews: vec![],
    };

    wince::assert_eq!(fetched, expected);
}

#[tokio::test]
async fn a_comment_by_a_deleted_account_maps_to_a_ghost_author() {
    let server = MockServer::start().await;

    // GitHub reports a null user for a comment whose author deleted their
    // account; the fetch stands rather than failing over the missing author.
    let comment = review_comment_fixture(
        100,
        "Left by someone since departed.",
        Value::Null,
        json!({}),
    );
    let github = github_serving(&server, base_pull(), json!([comment]), json!([]), json!([])).await;
    let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

    let fetched = github.fetch(&url).await.expect("fetch succeeds");

    let expected = expected_shell(
        &url,
        vec![FetchedComment {
            origin: review_comment_ref(100),
            author: Author {
                name: "ghost".to_string(),
                kind: AuthorKind::Human,
            },
            body: "Left by someone since departed.".to_string(),
            authored_at: datetime!(2021-06-01 12:00:00 UTC),
            anchor: Some(ForgeAnchor {
                path: "src/lib.rs".to_string(),
                side: Side::After,
                start_line: LineNo::new(42).expect("nonzero line"),
                end_line: LineNo::new(42).expect("nonzero line"),
                commit: RevisionId(HEAD_SHA.to_string()),
            }),
            reply_to: None,
            resolution: None,
        }],
    );

    wince::assert_eq!(fetched, expected);
}

#[tokio::test]
async fn a_multi_line_comment_crossing_sides_anchors_to_its_end_line() {
    let server = MockServer::start().await;

    // The comment starts on the deletion side and ends on the addition side; a
    // single-side anchor cannot span both, so it collapses onto the end line.
    let comment = review_comment_fixture(
        100,
        "A note spanning both sides of the hunk.",
        account("reviewer", "User"),
        json!({
            "start_line": 8,
            "start_side": "LEFT",
            "line": 42,
            "side": "RIGHT",
        }),
    );
    let github = github_serving(&server, base_pull(), json!([comment]), json!([]), json!([])).await;
    let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

    let fetched = github.fetch(&url).await.expect("fetch succeeds");

    let expected = expected_shell(
        &url,
        vec![FetchedComment {
            origin: review_comment_ref(100),
            author: Author {
                name: "reviewer".to_string(),
                kind: AuthorKind::Human,
            },
            body: "A note spanning both sides of the hunk.".to_string(),
            authored_at: datetime!(2021-06-01 12:00:00 UTC),
            anchor: Some(ForgeAnchor {
                path: "src/lib.rs".to_string(),
                side: Side::After,
                start_line: LineNo::new(42).expect("nonzero line"),
                end_line: LineNo::new(42).expect("nonzero line"),
                commit: RevisionId(HEAD_SHA.to_string()),
            }),
            reply_to: None,
            resolution: None,
        }],
    );

    wince::assert_eq!(fetched, expected);
}

#[tokio::test]
async fn a_comment_whose_start_line_is_below_its_end_is_rejected() {
    let server = MockServer::start().await;

    // A same-side range with start_line past line breaks the anchor invariant.
    let comment = review_comment_fixture(
        100,
        "A backwards range.",
        account("reviewer", "User"),
        json!({
            "start_line": 50,
            "start_side": "RIGHT",
            "line": 42,
            "side": "RIGHT",
        }),
    );
    let github = github_serving(&server, base_pull(), json!([comment]), json!([]), json!([])).await;
    let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

    let error = github
        .fetch(&url)
        .await
        .expect_err("a backwards range is rejected");

    wince::assert_eq!(
        error.to_string(),
        "a github review comment starts below where it ends".to_string()
    );
}

#[tokio::test]
async fn a_resolved_thread_marks_its_root_comment() {
    let server = MockServer::start().await;

    // Comment 100 roots a resolved thread; comment 101 roots an open one. Only
    // the resolved thread's root gets a resolution, named by its resolver.
    let resolved = review_comment_fixture(
        100,
        "Please fix this.",
        account("reviewer", "User"),
        json!({}),
    );
    let open = review_comment_fixture(101, "And this too.", account("reviewer", "User"), json!({}));
    let threads = json!([
        {
            "id": "PRRT_kwthread1",
            "isResolved": true,
            "resolvedBy": { "login": "maintainer" },
            "comments": { "nodes": [ { "databaseId": 100 } ] },
        },
        {
            "id": "PRRT_kwthread2",
            "isResolved": false,
            "resolvedBy": null,
            "comments": { "nodes": [ { "databaseId": 101 } ] },
        },
    ]);
    let github = github_serving_with_threads(
        &server,
        base_pull(),
        json!([resolved, open]),
        json!([]),
        json!([]),
        threads,
    )
    .await;
    let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

    let fetched = github.fetch(&url).await.expect("fetch succeeds");

    let anchor = || {
        Some(ForgeAnchor {
            path: "src/lib.rs".to_string(),
            side: Side::After,
            start_line: LineNo::new(42).expect("nonzero line"),
            end_line: LineNo::new(42).expect("nonzero line"),
            commit: RevisionId(HEAD_SHA.to_string()),
        })
    };
    let expected = expected_shell(
        &url,
        vec![
            FetchedComment {
                origin: review_comment_ref(100),
                author: Author {
                    name: "reviewer".to_string(),
                    kind: AuthorKind::Human,
                },
                body: "Please fix this.".to_string(),
                authored_at: datetime!(2021-06-01 12:00:00 UTC),
                anchor: anchor(),
                reply_to: None,
                resolution: Some(Resolution {
                    by: Some(Author {
                        name: "maintainer".to_string(),
                        kind: AuthorKind::Human,
                    }),
                }),
            },
            FetchedComment {
                origin: review_comment_ref(101),
                author: Author {
                    name: "reviewer".to_string(),
                    kind: AuthorKind::Human,
                },
                body: "And this too.".to_string(),
                authored_at: datetime!(2021-06-01 12:00:00 UTC),
                anchor: anchor(),
                reply_to: None,
                resolution: None,
            },
        ],
    );

    wince::assert_eq!(fetched, expected);
}

#[tokio::test]
async fn set_resolved_resolves_the_thread_holding_a_comment() {
    let server = MockServer::start().await;

    // The lookup query finds the thread holding comment 100; the resolve
    // mutation must run exactly once, addressing that thread by its node id.
    let threads = threads_response(json!([
        {
            "id": "PRRT_kwthread1",
            "isResolved": false,
            "resolvedBy": null,
            "comments": { "nodes": [ { "databaseId": 100 } ] },
        }
    ]));
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviewThreads("))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("resolveReviewThread"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "resolveReviewThread": { "thread": { "id": "PRRT_kwthread1" } } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let github = GithubForge::new("token", Some(&server.uri())).expect("build the adapter");
    let at = ExternalRef {
        forge: ForgeId {
            provider: "github".to_string(),
            host: "github.com".to_string(),
        },
        kind: ExternalKind::ReviewComment,
        id: "100".to_string(),
        url: Some("https://github.com/octo/demo/pull/7#discussion_r100".to_string()),
    };

    github
        .set_resolved(&at, true)
        .await
        .expect("resolve succeeds");
    // The mounted mutation's expect(1) is verified when the server drops.
}

#[tokio::test]
async fn set_resolved_errors_when_no_thread_holds_the_comment() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads_response(json!([]))))
        .mount(&server)
        .await;

    let github = GithubForge::new("token", Some(&server.uri())).expect("build the adapter");
    let at = ExternalRef {
        forge: ForgeId {
            provider: "github".to_string(),
            host: "github.com".to_string(),
        },
        kind: ExternalKind::ReviewComment,
        id: "100".to_string(),
        url: Some("https://github.com/octo/demo/pull/7#discussion_r100".to_string()),
    };

    let error = github
        .set_resolved(&at, true)
        .await
        .expect_err("an unknown comment is rejected");

    wince::assert_eq!(
        error.to_string(),
        "no review thread holds comment 100".to_string()
    );
}

#[tokio::test]
async fn set_resolved_locates_a_thread_from_a_reply_not_its_root() {
    let server = MockServer::start().await;

    // Comment 101 is a reply, not the thread's root (comment 100). Resolving it
    // must still find the thread it sits in, since wiff resolves any comment.
    let threads = threads_response(json!([
        {
            "id": "PRRT_kwthread1",
            "isResolved": false,
            "resolvedBy": null,
            "comments": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [ { "databaseId": 100 }, { "databaseId": 101 } ],
            },
        }
    ]));
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviewThreads("))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("resolveReviewThread"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "resolveReviewThread": { "thread": { "id": "PRRT_kwthread1" } } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let github = GithubForge::new("token", Some(&server.uri())).expect("build the adapter");
    let at = ExternalRef {
        forge: ForgeId {
            provider: "github".to_string(),
            host: "github.com".to_string(),
        },
        kind: ExternalKind::ReviewComment,
        id: "101".to_string(),
        url: Some("https://github.com/octo/demo/pull/7#discussion_r101".to_string()),
    };

    github
        .set_resolved(&at, true)
        .await
        .expect("resolve succeeds");
    // The mounted mutation's expect(1) is verified when the server drops.
}

#[tokio::test]
async fn set_resolved_pages_a_thread_whose_comment_overflows_the_first_page() {
    let server = MockServer::start().await;

    // The target comment 250 sits past the first page of the thread's comments,
    // so the lookup must follow the thread's comment cursor to find it.
    let threads = threads_response(json!([
        {
            "id": "PRRT_kwthread1",
            "isResolved": false,
            "resolvedBy": null,
            "comments": {
                "pageInfo": { "hasNextPage": true, "endCursor": "CURSOR1" },
                "nodes": [ { "databaseId": 100 } ],
            },
        }
    ]));
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviewThreads("))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("PullRequestReviewThread"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "node": { "comments": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [ { "databaseId": 250 } ],
            } } }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("resolveReviewThread"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "resolveReviewThread": { "thread": { "id": "PRRT_kwthread1" } } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let github = GithubForge::new("token", Some(&server.uri())).expect("build the adapter");
    let at = ExternalRef {
        forge: ForgeId {
            provider: "github".to_string(),
            host: "github.com".to_string(),
        },
        kind: ExternalKind::ReviewComment,
        id: "250".to_string(),
        url: Some("https://github.com/octo/demo/pull/7#discussion_r250".to_string()),
    };

    github
        .set_resolved(&at, true)
        .await
        .expect("resolve succeeds");
    // The mounted mutation's expect(1) is verified when the server drops.
}

#[tokio::test]
async fn a_bot_resolved_thread_attributes_an_agent() {
    let server = MockServer::start().await;

    // GitHub's resolvedBy is an actor: a GitHub App reports __typename "Bot",
    // which maps to an agent the way a bot comment author does.
    let comment = review_comment_fixture(
        100,
        "Please fix this.",
        account("reviewer", "User"),
        json!({}),
    );
    let threads = json!([
        {
            "id": "PRRT_kwthread1",
            "isResolved": true,
            "resolvedBy": { "login": "dependabot", "__typename": "Bot" },
            "comments": { "nodes": [ { "databaseId": 100 } ] },
        }
    ]);
    let github = github_serving_with_threads(
        &server,
        base_pull(),
        json!([comment]),
        json!([]),
        json!([]),
        threads,
    )
    .await;
    let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

    let fetched = github.fetch(&url).await.expect("fetch succeeds");

    let expected = expected_shell(
        &url,
        vec![FetchedComment {
            origin: review_comment_ref(100),
            author: Author {
                name: "reviewer".to_string(),
                kind: AuthorKind::Human,
            },
            body: "Please fix this.".to_string(),
            authored_at: datetime!(2021-06-01 12:00:00 UTC),
            anchor: Some(ForgeAnchor {
                path: "src/lib.rs".to_string(),
                side: Side::After,
                start_line: LineNo::new(42).expect("nonzero line"),
                end_line: LineNo::new(42).expect("nonzero line"),
                commit: RevisionId(HEAD_SHA.to_string()),
            }),
            reply_to: None,
            resolution: Some(Resolution {
                by: Some(Author {
                    name: "dependabot".to_string(),
                    kind: AuthorKind::Agent,
                }),
            }),
        }],
    );

    wince::assert_eq!(fetched, expected);
}

#[tokio::test]
async fn a_graphql_failure_leaves_the_rest_fetch_intact() {
    let server = MockServer::start().await;

    // Thread resolution is additive metadata read over GraphQL; a GraphQL
    // outage must not abort a fetch the REST endpoints already satisfied. The
    // comment imports with no resolution rather than the whole fetch failing.
    let comment = review_comment_fixture(
        100,
        "Please fix this.",
        account("reviewer", "User"),
        json!({}),
    );
    for (route, body) in [
        ("/repos/octo/demo/pulls/7", base_pull()),
        ("/repos/octo/demo/pulls/7/comments", json!([comment])),
        ("/repos/octo/demo/issues/7/comments", json!([])),
        ("/repos/octo/demo/pulls/7/reviews", json!([])),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let github = GithubForge::new("token", Some(&server.uri())).expect("build the adapter");
    let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

    let fetched = github.fetch(&url).await.expect("fetch succeeds");

    let expected = expected_shell(
        &url,
        vec![FetchedComment {
            origin: review_comment_ref(100),
            author: Author {
                name: "reviewer".to_string(),
                kind: AuthorKind::Human,
            },
            body: "Please fix this.".to_string(),
            authored_at: datetime!(2021-06-01 12:00:00 UTC),
            anchor: Some(ForgeAnchor {
                path: "src/lib.rs".to_string(),
                side: Side::After,
                start_line: LineNo::new(42).expect("nonzero line"),
                end_line: LineNo::new(42).expect("nonzero line"),
                commit: RevisionId(HEAD_SHA.to_string()),
            }),
            reply_to: None,
            resolution: None,
        }],
    );

    wince::assert_eq!(fetched, expected);
}
