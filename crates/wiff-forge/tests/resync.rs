#![allow(missing_docs)]

use std::collections::HashMap;

use time::macros::datetime;
use ulid::Ulid;
use wiff_core::comment::edit_event;
use wiff_core::identity::ProjectIdentity;
use wiff_core::record::{
    Author, AuthorKind, CommentTarget, Description, Disposition, ExternalKind, ExternalRef,
    ForgeId, ForgeUrl, RevisionId, ScmSource, SourceKind, TipRule,
};
use wiff_core::review::{CommentState, ReviewState};
use wiff_core::session::SessionLog;
use wiff_core::source::{CapturedDiff, FetchSource};
use wiff_core::{BaseRuleset, RefreshOutcome, ScmType, SessionId};
use wiff_diff::{LineNo, Side};
use wiff_forge::types::{
    FetchedComment, FetchedDescription, FetchedPullRequest, FetchedReview, ForgeAnchor,
};
use wiff_forge::{ImportRequest, ResyncOutcome, import_pull_request, resync_pull_request};

/// The diff a session is first imported from: the eleventh line of `f.txt`
/// uppercased.
const DIFF_V0: &str = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -10,4 +10,4 @@
 ten
-eleven
+ELEVEN
 twelve
 thirteen
";

/// The diff a later pull captures: the `f.txt` change is unchanged, so the
/// imported inline comment rebases onto it cleanly, and a second file `g.txt`
/// has gained a change for a new inline comment to anchor against.
const DIFF_V1: &str = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -10,4 +10,4 @@
 ten
-eleven
+ELEVEN
 twelve
 thirteen
diff --git a/g.txt b/g.txt
--- a/g.txt
+++ b/g.txt
@@ -1,2 +1,2 @@
-old
+new
 tail
";

fn human(name: &str) -> Author {
    Author {
        name: name.to_string(),
        kind: AuthorKind::Human,
    }
}

fn forge_id() -> ForgeId {
    ForgeId {
        provider: "github".to_string(),
        host: "github.com".to_string(),
    }
}

fn origin(kind: ExternalKind, id: &str) -> ExternalRef {
    ExternalRef {
        forge: forge_id(),
        kind,
        id: id.to_string(),
        url: None,
    }
}

fn pr_url() -> ForgeUrl {
    ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url")
}

fn identity() -> ProjectIdentity {
    ProjectIdentity {
        canonical: "demo".to_string(),
        repo_root: None,
        scm: Some(ScmType::Git),
    }
}

/// A captured diff over a merge-base ruleset, standing in for what an in-repo
/// caller's `GitSource` over the pinned commits produces.
fn captured(text: &str, head: &str) -> CapturedDiff {
    CapturedDiff {
        text: text.to_string(),
        source: SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("merge-base(name(base))"),
            tip: TipRule::Pinned {
                revision: RevisionId(head.to_string()),
            },
            branch_hint: None,
        }),
        base_revision: Some(RevisionId("mbase".to_string())),
        base_tip_relative: false,
        head_revision: Some(RevisionId(head.to_string())),
    }
}

fn inline_comment(commit: &str) -> FetchedComment {
    FetchedComment {
        origin: origin(ExternalKind::ReviewComment, "c-inline"),
        author: human("octocat"),
        body: "should this be lowercase?".to_string(),
        authored_at: datetime!(2024-03-01 12:00 UTC),
        anchor: Some(ForgeAnchor {
            path: "f.txt".to_string(),
            side: Side::After,
            start_line: LineNo::new(11).unwrap(),
            end_line: LineNo::new(11).unwrap(),
            commit: RevisionId(commit.to_string()),
        }),
        reply_to: None,
        resolution: None,
    }
}

fn approving_review() -> FetchedReview {
    FetchedReview {
        origin: origin(ExternalKind::Verdict, "r-1"),
        author: human("octocat"),
        body: "looks good to me".to_string(),
        authored_at: datetime!(2024-03-01 12:15 UTC),
        disposition: Some(Disposition::Approve),
        dismissed: false,
    }
}

/// The pull request at first import: a description, one inline comment on the
/// uppercased line, and one approving review.
fn fetched_v0() -> FetchedPullRequest {
    FetchedPullRequest {
        url: pr_url(),
        description: FetchedDescription {
            origin: origin(ExternalKind::Description, "pull-body"),
            author: human("wez"),
            content: Description {
                title: "Uppercase the eleventh line".to_string(),
                body: "A deliberate shout.".to_string(),
            },
            authored_at: datetime!(2024-03-01 11:00 UTC),
        },
        head: FetchSource::Git {
            url: "https://github.com/octo/demo".to_string(),
            git_ref: "refs/pull/7/head".to_string(),
            commit: RevisionId("head0".to_string()),
        },
        base: FetchSource::Git {
            url: "https://github.com/octo/demo".to_string(),
            git_ref: "refs/heads/main".to_string(),
            commit: RevisionId("mbase".to_string()),
        },
        comments: vec![inline_comment("head0")],
        reviews: vec![approving_review()],
    }
}

/// The pull request as a later pull sees it: the description reworded, the
/// inline comment's body edited upstream, a new inline comment on `g.txt`, and
/// the review unchanged.
fn fetched_v1() -> FetchedPullRequest {
    let inline = FetchedComment {
        body: "did you mean to shout here?".to_string(),
        authored_at: datetime!(2024-03-02 09:00 UTC),
        ..inline_comment("head1")
    };
    let on_g = FetchedComment {
        origin: origin(ExternalKind::ReviewComment, "c-g"),
        author: human("wez"),
        body: "this rename looks right".to_string(),
        authored_at: datetime!(2024-03-02 09:05 UTC),
        anchor: Some(ForgeAnchor {
            path: "g.txt".to_string(),
            side: Side::After,
            start_line: LineNo::new(1).unwrap(),
            end_line: LineNo::new(1).unwrap(),
            commit: RevisionId("head1".to_string()),
        }),
        reply_to: None,
        resolution: None,
    };
    FetchedPullRequest {
        description: FetchedDescription {
            origin: origin(ExternalKind::Description, "pull-body"),
            author: human("wez"),
            content: Description {
                title: "Uppercase the eleventh line".to_string(),
                body: "A deliberate shout, in all caps.".to_string(),
            },
            authored_at: datetime!(2024-03-02 08:30 UTC),
        },
        head: FetchSource::Git {
            url: "https://github.com/octo/demo".to_string(),
            git_ref: "refs/pull/7/head".to_string(),
            commit: RevisionId("head1".to_string()),
        },
        comments: vec![inline, on_g],
        reviews: vec![approving_review()],
        ..fetched_v0()
    }
}

/// Import `fetched` as a fresh bound session under a fixed id and return the
/// session's path and an open log over it.
async fn seed(
    base: &std::path::Path,
    fetched: &FetchedPullRequest,
) -> (std::path::PathBuf, SessionLog) {
    let identity = identity();
    let session: SessionId = "000000001".parse().unwrap();
    import_pull_request(
        &captured(DIFF_V0, "head0"),
        fetched,
        &ImportRequest {
            session,
            base,
            identity: &identity,
            cwd: std::path::Path::new("/work"),
        },
    )
    .await
    .expect("import succeeds");
    let path = base
        .join("sessions")
        .join("demo")
        .join(format!("{session}.jsonl"));
    let log = SessionLog::open(&path).expect("open session");
    (path, log)
}

/// The local id of the comment mirroring the forge object numbered `origin_id`.
fn comment_id(state: &ReviewState, origin_id: &str) -> Ulid {
    state
        .comments
        .iter()
        .find(|comment| {
            comment
                .origin
                .as_ref()
                .is_some_and(|origin| origin.id == origin_id)
        })
        .map(|comment| comment.id)
        .expect("a comment mirroring that origin")
}

/// Render a folded review into a stable text form, mapping each comment's random
/// id to a token by first appearance so the assertion does not depend on the
/// minted ids.
fn render(state: &ReviewState) -> String {
    let mut tokens: HashMap<Ulid, String> = HashMap::new();
    for comment in &state.comments {
        let next = format!("c{}", tokens.len() + 1);
        tokens.entry(comment.id).or_insert(next);
    }

    let mut out = String::new();
    let version = state.latest_version().expect("a captured version");
    out.push_str(&format!(
        "session forge={}\n",
        state
            .session
            .forge
            .as_ref()
            .map(ForgeUrl::as_str)
            .unwrap_or("-")
    ));
    out.push_str(&format!(
        "v{} base={} head={}\n",
        version.number,
        version
            .base_revision
            .as_ref()
            .map(RevisionId::as_str)
            .unwrap_or("-"),
        version
            .head_revision
            .as_ref()
            .map(RevisionId::as_str)
            .unwrap_or("-"),
    ));
    if let Some(description) = &state.description {
        out.push_str(&format!(
            "description title={:?} body={:?} author={} origin={}\n",
            description.content.title,
            description.content.body,
            description.author.name,
            description
                .origin
                .as_ref()
                .map(|o| o.id.as_str())
                .unwrap_or("-"),
        ));
    }
    for comment in &state.comments {
        out.push_str(&format!(
            "{} author={} target={} body={:?} disposition={} resolved={} deleted={} origin={}\n",
            tokens[&comment.id],
            comment.author.name,
            target(comment, &tokens),
            comment.body,
            comment.disposition.map(|d| d.as_str()).unwrap_or("-"),
            comment.resolved,
            comment.deleted,
            comment
                .origin
                .as_ref()
                .map(|o| o.id.as_str())
                .unwrap_or("-"),
        ));
    }
    out
}

/// Describe a comment's target, resolving a reply's parent to its token.
fn target(comment: &CommentState, tokens: &HashMap<Ulid, String>) -> String {
    match &comment.target {
        CommentTarget::Review => "review".to_string(),
        CommentTarget::File { file } => format!("file:{file}"),
        CommentTarget::Lines {
            file,
            side,
            start_line,
            end_line,
        } => format!("lines:{file}:{side:?}:{start_line}-{end_line}"),
        CommentTarget::Comment { id } => {
            format!(
                "reply-to={}",
                tokens.get(id).map(String::as_str).unwrap_or("?")
            )
        }
    }
}

#[tokio::test]
async fn resyncing_recaptures_the_diff_and_reconciles_upstream_changes() {
    let base = tempfile::tempdir().expect("tempdir");
    let (path, mut log) = seed(base.path(), &fetched_v0()).await;

    let outcome = resync_pull_request(
        &mut log,
        &captured(DIFF_V1, "head1"),
        &fetched_v1(),
        human("wez"),
    )
    .await
    .expect("resync succeeds");

    // The imported inline comment rebased onto the unchanged f.txt hunk cleanly
    // (one exact move). The reconcile edited that comment's body and created the
    // new g.txt comment (two comments touched), left the review unchanged, and
    // imported the reworded description.
    wince::assert_eq!(
        outcome,
        ResyncOutcome {
            refresh: Some(RefreshOutcome {
                version: wiff_core::record::VersionNumber(1),
                exact: 1,
                approximate: 0,
                relocated: 0,
                outdated: 0,
                base_shift: None,
            }),
            comments: 2,
            reviews: 0,
            description_updated: true,
        }
    );

    let state = ReviewState::load(&path).expect("fold");
    wince::snapshot_str!(
        render(&state),
        "session forge=https://github.com/octo/demo/pull/7\n\
         v1 base=mbase head=head1\n\
         description title=\"Uppercase the eleventh line\" body=\"A deliberate shout, in all caps.\" author=wez origin=pull-body\n\
         c1 author=octocat target=lines:f.txt:After:11-11 body=\"did you mean to shout here?\" disposition=- resolved=false deleted=false origin=c-inline\n\
         c2 author=octocat target=review body=\"looks good to me\" disposition=approve resolved=false deleted=false origin=r-1\n\
         c3 author=wez target=lines:g.txt:After:1-1 body=\"this rename looks right\" disposition=- resolved=false deleted=false origin=c-g\n"
    );
}

#[tokio::test]
async fn resyncing_withdraws_a_comment_the_forge_dropped() {
    let base = tempfile::tempdir().expect("tempdir");
    let (path, mut log) = seed(base.path(), &fetched_v0()).await;

    // The forge no longer lists the inline comment; the diff and everything else
    // are unchanged.
    let dropped = FetchedPullRequest {
        comments: vec![],
        ..fetched_v0()
    };
    let outcome = resync_pull_request(
        &mut log,
        &captured(DIFF_V0, "head0"),
        &dropped,
        human("wez"),
    )
    .await
    .expect("resync succeeds");

    wince::assert_eq!(
        outcome,
        ResyncOutcome {
            refresh: None,
            comments: 1,
            reviews: 0,
            description_updated: false,
        }
    );

    // The withdrawn comment is tombstoned in an unknown hand rather than removed.
    let state = ReviewState::load(&path).expect("fold");
    wince::snapshot_str!(
        render(&state),
        "session forge=https://github.com/octo/demo/pull/7\n\
         v0 base=mbase head=head0\n\
         description title=\"Uppercase the eleventh line\" body=\"A deliberate shout.\" author=wez origin=pull-body\n\
         c1 author=octocat target=lines:f.txt:After:11-11 body=\"should this be lowercase?\" disposition=- resolved=false deleted=true origin=c-inline\n\
         c2 author=octocat target=review body=\"looks good to me\" disposition=approve resolved=false deleted=false origin=r-1\n"
    );
}

#[tokio::test]
async fn resyncing_preserves_a_local_edit_made_against_unchanged_upstream() {
    let base = tempfile::tempdir().expect("tempdir");
    let (path, mut log) = seed(base.path(), &fetched_v0()).await;

    // The reviewer edits the imported comment locally before pulling again.
    let seeded = ReviewState::load(&path).expect("fold");
    let id = comment_id(&seeded, "c-inline");
    log.append_locked(edit_event(
        id,
        human("wez"),
        "actually this reads fine".to_string(),
    ))
    .expect("local edit");

    // The next pull sees the same upstream state it was imported from.
    let outcome = resync_pull_request(
        &mut log,
        &captured(DIFF_V0, "head0"),
        &fetched_v0(),
        human("wez"),
    )
    .await
    .expect("resync succeeds");

    // Nothing upstream changed since the last sync, so the reconcile leaves the
    // unpushed local edit intact rather than importing over it.
    wince::assert_eq!(
        outcome,
        ResyncOutcome {
            refresh: None,
            comments: 0,
            reviews: 0,
            description_updated: false,
        }
    );

    let state = ReviewState::load(&path).expect("fold");
    wince::snapshot_str!(
        render(&state),
        "session forge=https://github.com/octo/demo/pull/7\n\
         v0 base=mbase head=head0\n\
         description title=\"Uppercase the eleventh line\" body=\"A deliberate shout.\" author=wez origin=pull-body\n\
         c1 author=octocat target=lines:f.txt:After:11-11 body=\"actually this reads fine\" disposition=- resolved=false deleted=false origin=c-inline\n\
         c2 author=octocat target=review body=\"looks good to me\" disposition=approve resolved=false deleted=false origin=r-1\n"
    );
}

#[tokio::test]
async fn resyncing_clears_a_dismissed_reviews_verdict() {
    let base = tempfile::tempdir().expect("tempdir");
    let (path, mut log) = seed(base.path(), &fetched_v0()).await;

    // The approving review is dismissed upstream; the forge still lists it, now
    // with its verdict cleared.
    let dismissed = FetchedPullRequest {
        reviews: vec![FetchedReview {
            disposition: None,
            dismissed: true,
            ..approving_review()
        }],
        ..fetched_v0()
    };
    let outcome = resync_pull_request(
        &mut log,
        &captured(DIFF_V0, "head0"),
        &dismissed,
        human("wez"),
    )
    .await
    .expect("resync succeeds");

    wince::assert_eq!(
        outcome,
        ResyncOutcome {
            refresh: None,
            comments: 0,
            reviews: 1,
            description_updated: false,
        }
    );

    // The mirrored review keeps its summary but no longer holds a verdict.
    let state = ReviewState::load(&path).expect("fold");
    wince::snapshot_str!(
        render(&state),
        "session forge=https://github.com/octo/demo/pull/7\n\
         v0 base=mbase head=head0\n\
         description title=\"Uppercase the eleventh line\" body=\"A deliberate shout.\" author=wez origin=pull-body\n\
         c1 author=octocat target=lines:f.txt:After:11-11 body=\"should this be lowercase?\" disposition=- resolved=false deleted=false origin=c-inline\n\
         c2 author=octocat target=review body=\"looks good to me\" disposition=- resolved=false deleted=false origin=r-1\n"
    );
}

#[tokio::test]
async fn resyncing_rejects_a_fetch_for_a_different_pull_request() {
    let base = tempfile::tempdir().expect("tempdir");
    let (_path, mut log) = seed(base.path(), &fetched_v0()).await;

    let elsewhere = FetchedPullRequest {
        url: ForgeUrl::parse("https://github.com/octo/demo/pull/99").expect("valid url"),
        ..fetched_v0()
    };
    let error = resync_pull_request(
        &mut log,
        &captured(DIFF_V0, "head0"),
        &elsewhere,
        human("wez"),
    )
    .await
    .expect_err("a mismatched pull request is rejected");

    wince::assert_eq!(
        error.to_string(),
        "session 000000001 is bound to https://github.com/octo/demo/pull/7, not https://github.com/octo/demo/pull/99"
    );
}
