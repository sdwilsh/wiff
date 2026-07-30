#![allow(missing_docs)]

use std::collections::HashMap;

use time::macros::datetime;
use ulid::Ulid;
use wiff_core::identity::ProjectIdentity;
use wiff_core::record::{
    Author, AuthorKind, CommentTarget, Description, Disposition, ExternalKind, ExternalRef,
    ForgeId, ForgeUrl, RevisionId, ScmSource, SourceKind, TipRule,
};
use wiff_core::review::{CommentState, ReviewState};
use wiff_core::source::{CapturedDiff, FetchSource};
use wiff_core::{BaseRuleset, ScmType, SessionId};
use wiff_diff::{LineNo, Side};
use wiff_forge::types::{
    FetchedComment, FetchedDescription, FetchedPullRequest, FetchedReview, ForgeAnchor, Resolution,
};
use wiff_forge::{ImportOutcome, ImportRequest, import_pull_request};

const DIFF: &str = "\
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

/// The captured `v0` diff a session is created from, standing in for what an
/// in-repo caller's `GitSource` over the pinned commits would produce.
fn captured() -> CapturedDiff {
    CapturedDiff {
        text: DIFF.to_string(),
        source: SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("merge-base(name(base))"),
            tip: TipRule::Pinned {
                revision: RevisionId("head".to_string()),
            },
            branch_hint: None,
        }),
        base_revision: Some(RevisionId("mbase".to_string())),
        base_tip_relative: false,
        head_revision: Some(RevisionId("head".to_string())),
    }
}

/// A pull request whose description, one inline comment with a reply, one
/// review-level comment, and one approving review the import mirrors.
fn fetched() -> FetchedPullRequest {
    let inline = FetchedComment {
        origin: origin(ExternalKind::ReviewComment, "c-inline"),
        author: human("octocat"),
        body: "should this be lowercase?".to_string(),
        authored_at: datetime!(2024-03-01 12:00 UTC),
        anchor: Some(ForgeAnchor {
            path: "f.txt".to_string(),
            side: Side::After,
            start_line: LineNo::new(11).unwrap(),
            end_line: LineNo::new(11).unwrap(),
            commit: RevisionId("head".to_string()),
        }),
        reply_to: None,
        resolution: None,
    };
    let reply = FetchedComment {
        origin: origin(ExternalKind::ReviewComment, "c-reply"),
        author: human("wez"),
        body: "no, the caps are deliberate".to_string(),
        authored_at: datetime!(2024-03-01 12:05 UTC),
        anchor: None,
        reply_to: Some(origin(ExternalKind::ReviewComment, "c-inline")),
        resolution: Some(Resolution {
            by: Some(human("wez")),
        }),
    };
    let overall = FetchedComment {
        origin: origin(ExternalKind::ReviewComment, "c-overall"),
        author: human("octocat"),
        body: "thanks for the fix".to_string(),
        authored_at: datetime!(2024-03-01 12:10 UTC),
        anchor: None,
        reply_to: None,
        resolution: None,
    };
    let verdict = FetchedReview {
        origin: origin(ExternalKind::Verdict, "r-1"),
        author: human("octocat"),
        body: "looks good to me".to_string(),
        authored_at: datetime!(2024-03-01 12:15 UTC),
        disposition: Some(Disposition::Approve),
        dismissed: false,
    };
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
            commit: RevisionId("head".to_string()),
        },
        base: FetchSource::Git {
            url: "https://github.com/octo/demo".to_string(),
            git_ref: "refs/heads/main".to_string(),
            commit: RevisionId("basetip".to_string()),
        },
        comments: vec![inline, reply, overall],
        reviews: vec![verdict],
    }
}

/// Render a folded review into a stable text form, mapping each comment's random
/// id to a token by first appearance so threading is asserted without depending
/// on the minted ids.
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
            "{} author={} target={} body={:?} disposition={} resolved={} origin={}\n",
            tokens[&comment.id],
            comment.author.name,
            target(comment, &tokens),
            comment.body,
            comment.disposition.map(|d| d.as_str()).unwrap_or("-"),
            comment.resolved,
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
async fn importing_a_pull_request_binds_a_session_and_mirrors_its_metadata() {
    let base = tempfile::tempdir().expect("tempdir");
    let source = captured();
    let fetched = fetched();
    let session: SessionId = "000000001".parse().unwrap();
    let identity = identity();
    let req = ImportRequest {
        session,
        base: base.path(),
        identity: &identity,
        cwd: std::path::Path::new("/work"),
    };

    let outcome = import_pull_request(&source, &fetched, &req)
        .await
        .expect("import succeeds");

    wince::assert_eq!(
        outcome,
        ImportOutcome {
            session,
            version: wiff_core::record::VersionNumber(0),
            comments: 3,
            reviews: 1,
            description_imported: true,
        }
    );

    let state = ReviewState::load(
        &base
            .path()
            .join("sessions")
            .join("demo")
            .join(format!("{session}.jsonl")),
    )
    .expect("fold");
    wince::snapshot_str!(
        render(&state),
        "session forge=https://github.com/octo/demo/pull/7\n\
         v0 base=mbase head=head\n\
         description title=\"Uppercase the eleventh line\" body=\"A deliberate shout.\" author=wez origin=pull-body\n\
         c1 author=octocat target=lines:f.txt:After:11-11 body=\"should this be lowercase?\" disposition=- resolved=false origin=c-inline\n\
         c2 author=wez target=reply-to=c1 body=\"no, the caps are deliberate\" disposition=- resolved=true origin=c-reply\n\
         c3 author=octocat target=review body=\"thanks for the fix\" disposition=- resolved=false origin=c-overall\n\
         c4 author=octocat target=review body=\"looks good to me\" disposition=approve resolved=false origin=r-1\n"
    );
}
