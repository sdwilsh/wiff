#![allow(missing_docs)]

use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use ulid::Ulid;
use wiff_core::description::set_description;
use wiff_core::error::Result as CoreResult;
use wiff_core::record::{
    Author, AuthorKind, Description, ExternalRef, FORMAT_VERSION, ForgeUrl, RecordBody, RevisionId,
    ScmSource, SessionHeader, SourceKind, TipRule,
};
use wiff_core::review::ReviewState;
use wiff_core::session::{LockWait, SessionLog};
use wiff_core::source::{FetchSource, Remote, ScmRepo, TrackingBranch};
use wiff_core::{BaseRuleset, ScmType};
use wiff_forge::{
    FetchedPullRequest, Forge, NewPullRequest, OpenRefusal, OpenRequest, OpenedPullRequest,
    OutgoingComment, OutgoingReview, SubmittedReview, disambiguated_branch, open_pull_request,
};

fn human(name: &str) -> Author {
    Author {
        name: name.to_string(),
        kind: AuthorKind::Human,
    }
}

fn header(ulid: Ulid) -> SessionHeader {
    SessionHeader {
        ulid,
        version: FORMAT_VERSION,
        project: "demo".to_string(),
        repo_root: Some("/repos/demo".to_string()),
        cwd: "/repos/demo".to_string(),
        source: SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("ref(name(deadbeef))"),
            tip: TipRule::Worktree,
            branch_hint: None,
        }),
        forge: None,
    }
}

/// The repository the pull request opens in.
fn repo_url() -> ForgeUrl {
    ForgeUrl::parse("https://github.com/octo/demo").expect("valid url")
}

/// The reviewed commit to publish as the pull request's head.
fn head() -> RevisionId {
    RevisionId("cafe".to_string())
}

/// A real session whose log holds the header and, when `title` is `Some`, a
/// description revision, folded to a [`ReviewState`].
fn review(title: Option<&str>) -> (tempfile::TempDir, ReviewState) {
    let base = tempfile::tempdir().expect("tempdir");
    let (mut log, lock) = SessionLog::create(base.path(), "demo", |ulid| {
        RecordBody::Session(header(ulid))
    })
    .expect("create session");
    drop(lock);
    if let Some(title) = title {
        set_description(
            &mut log,
            Description {
                title: title.to_string(),
                body: "The body of the review.".to_string(),
            },
            human("wez"),
            LockWait::Block,
        )
        .expect("set description");
    }
    let state = ReviewState::load(log.path()).expect("fold");
    (base, state)
}

/// A forge that records the pull request it was asked to open and returns a
/// fixed URL. Only [`create_pull_request`](Forge::create_pull_request) is
/// reached; opening does not fetch or write comments.
struct FakeForge {
    opened: Mutex<Vec<String>>,
}

impl FakeForge {
    fn new() -> Self {
        Self {
            opened: Mutex::new(Vec::new()),
        }
    }

    fn opened(&self) -> String {
        self.opened.lock().expect("lock").join("\n")
    }
}

#[async_trait]
impl Forge for FakeForge {
    async fn fetch(&self, _pr: &ForgeUrl) -> Result<FetchedPullRequest> {
        unreachable!("opening a pull request does not fetch")
    }

    async fn submit_review(
        &self,
        _pr: &ForgeUrl,
        _review: &OutgoingReview,
    ) -> Result<SubmittedReview> {
        unreachable!("opening a pull request does not submit a review")
    }

    async fn post_comment(
        &self,
        _pr: &ForgeUrl,
        _comment: &OutgoingComment,
    ) -> Result<ExternalRef> {
        unreachable!("opening a pull request does not post comments")
    }

    async fn edit_comment(&self, _at: &ExternalRef, _body: &str) -> Result<()> {
        unreachable!("opening a pull request does not edit comments")
    }

    async fn set_resolved(&self, _at: &ExternalRef, _resolved: bool) -> Result<()> {
        unreachable!("opening a pull request does not resolve comments")
    }

    async fn set_description(&self, _pr: &ForgeUrl, _description: &Description) -> Result<()> {
        unreachable!("opening a pull request does not set a description")
    }

    async fn create_pull_request(&self, req: &NewPullRequest) -> Result<ForgeUrl> {
        self.opened.lock().expect("lock").push(format!(
            "repo={} title={:?} body={:?} head={} base={}",
            req.repo.as_str(),
            req.description.title,
            req.description.body,
            req.head_branch,
            req.base_branch
        ));
        Ok(ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url"))
    }

    fn pull_request_url(&self, _remote_url: &str, _id: &str) -> Result<ForgeUrl> {
        unreachable!("opening a pull request does not resolve one by id")
    }
}

/// A repository whose preflight answers are fixed by construction, recording the
/// branch it is asked to publish. Only the create preflight and publish are
/// reached; pinning and fetching are not.
struct FakeRepo {
    clean: bool,
    upstream: Option<TrackingBranch>,
    /// The branches already on the remote, each with the commit it points at.
    remote_branches: Vec<(String, RevisionId)>,
    published: Mutex<Vec<String>>,
}

impl FakeRepo {
    /// A clean repository with no upstream and no branches on the remote.
    fn ready() -> Self {
        Self {
            clean: true,
            upstream: None,
            remote_branches: Vec::new(),
            published: Mutex::new(Vec::new()),
        }
    }

    fn published(&self) -> String {
        self.published.lock().expect("lock").join("\n")
    }
}

#[async_trait]
impl ScmRepo for FakeRepo {
    async fn fetch_pinned(&self, _source: &FetchSource, _session: Ulid) -> CoreResult<RevisionId> {
        unreachable!("opening a pull request does not fetch")
    }

    async fn pin_base(&self, _commit: &RevisionId, _session: Ulid) -> CoreResult<()> {
        unreachable!("opening a pull request does not pin")
    }

    async fn remotes(&self) -> CoreResult<Vec<Remote>> {
        unreachable!("opening a pull request does not enumerate remotes")
    }

    async fn working_tree_is_clean(&self) -> CoreResult<bool> {
        Ok(self.clean)
    }

    async fn remote_branch(&self, _remote: &str, branch: &str) -> CoreResult<Option<RevisionId>> {
        Ok(self
            .remote_branches
            .iter()
            .find(|(name, _)| name == branch)
            .map(|(_, commit)| commit.clone()))
    }

    async fn current_upstream(&self) -> CoreResult<Option<TrackingBranch>> {
        Ok(self.upstream.clone())
    }

    async fn publish_branch(
        &self,
        remote: &str,
        branch: &str,
        commit: &RevisionId,
    ) -> CoreResult<()> {
        self.published
            .lock()
            .expect("lock")
            .push(format!("remote={remote} branch={branch} commit={commit}"));
        Ok(())
    }

    async fn remove_pins(&self, _session: Ulid) -> CoreResult<()> {
        unreachable!("opening a pull request does not remove pins")
    }
}

fn request<'a>(repo: &'a ForgeUrl, commit: &'a RevisionId) -> OpenRequest<'a> {
    OpenRequest {
        repo,
        remote: "origin",
        head_commit: commit,
        base_branch: "main",
    }
}

#[tokio::test]
async fn a_ready_review_publishes_the_slug_branch_and_opens_the_pull_request() {
    let (_base, state) = review(Some("Refactor the widget"));
    let forge = FakeForge::new();
    let repo = FakeRepo::ready();
    let (url, commit) = (repo_url(), head());

    let opened = open_pull_request(&forge, &repo, &state, &request(&url, &commit))
        .await
        .expect("open succeeds");

    wince::assert_eq!(
        opened,
        OpenedPullRequest {
            url: ForgeUrl::parse("https://github.com/octo/demo/pull/7").unwrap(),
            branch: "refactor-the-widget".to_string(),
        }
    );
    wince::snapshot_str!(
        repo.published(),
        "remote=origin branch=refactor-the-widget commit=cafe"
    );
    wince::snapshot_str!(
        forge.opened(),
        "repo=https://github.com/octo/demo title=\"Refactor the widget\" body=\"The body of the review.\" head=refactor-the-widget base=main"
    );
}

#[tokio::test]
async fn a_taken_slug_publishes_a_session_disambiguated_branch() {
    let (_base, state) = review(Some("Refactor the widget"));
    let forge = FakeForge::new();
    let mut repo = FakeRepo::ready();
    // A different commit already holds the slug branch, forcing disambiguation.
    repo.remote_branches = vec![(
        "refactor-the-widget".to_string(),
        RevisionId("beef".to_string()),
    )];
    let (url, commit) = (repo_url(), head());
    let expected = disambiguated_branch("refactor-the-widget", state.session.ulid);

    let opened = open_pull_request(&forge, &repo, &state, &request(&url, &commit))
        .await
        .expect("open succeeds");

    wince::assert_eq!(
        opened,
        OpenedPullRequest {
            url: ForgeUrl::parse("https://github.com/octo/demo/pull/7").unwrap(),
            branch: expected.clone(),
        }
    );
    wince::assert_eq!(
        (repo.published(), forge.opened()),
        (
            format!("remote=origin branch={expected} commit=cafe"),
            format!(
                "repo=https://github.com/octo/demo title=\"Refactor the widget\" \
                 body=\"The body of the review.\" head={expected} base=main"
            ),
        )
    );
}

#[tokio::test]
async fn a_re_run_reuses_a_branch_that_already_holds_the_reviewed_commit() {
    // An earlier run published the slug branch and set the upstream to it, then
    // failed to open the pull request. A re-run finds the branch holding the
    // reviewed commit, reuses it, treats its own upstream as no blocker, and
    // opens the pull request rather than orphaning a branch or duplicating it.
    let (_base, state) = review(Some("Refactor the widget"));
    let forge = FakeForge::new();
    let mut repo = FakeRepo::ready();
    repo.remote_branches = vec![("refactor-the-widget".to_string(), head())];
    repo.upstream = Some(TrackingBranch {
        remote: "origin".to_string(),
        branch: "refactor-the-widget".to_string(),
    });
    let (url, commit) = (repo_url(), head());

    let opened = open_pull_request(&forge, &repo, &state, &request(&url, &commit))
        .await
        .expect("open succeeds");

    wince::assert_eq!(
        opened,
        OpenedPullRequest {
            url: ForgeUrl::parse("https://github.com/octo/demo/pull/7").unwrap(),
            branch: "refactor-the-widget".to_string(),
        }
    );
    wince::assert_eq!(
        (repo.published(), forge.opened()),
        (
            "remote=origin branch=refactor-the-widget commit=cafe".to_string(),
            "repo=https://github.com/octo/demo title=\"Refactor the widget\" \
             body=\"The body of the review.\" head=refactor-the-widget base=main"
                .to_string(),
        )
    );
}

#[tokio::test]
async fn a_missing_description_is_refused_before_publishing() {
    let (_base, state) = review(None);
    let forge = FakeForge::new();
    let repo = FakeRepo::ready();
    let (url, commit) = (repo_url(), head());

    let refusal = open_pull_request(&forge, &repo, &state, &request(&url, &commit))
        .await
        .expect_err("no description is refused")
        .downcast::<OpenRefusal>()
        .expect("an OpenRefusal");

    wince::assert_eq!(
        (refusal, repo.published(), forge.opened()),
        (OpenRefusal::NoDescription, String::new(), String::new())
    );
}

#[tokio::test]
async fn a_title_with_no_usable_characters_is_refused_before_publishing() {
    let (_base, state) = review(Some("--- !!! ---"));
    let forge = FakeForge::new();
    let repo = FakeRepo::ready();
    let (url, commit) = (repo_url(), head());

    let refusal = open_pull_request(&forge, &repo, &state, &request(&url, &commit))
        .await
        .expect_err("an unusable title is refused")
        .downcast::<OpenRefusal>()
        .expect("an OpenRefusal");

    wince::assert_eq!(
        (refusal, repo.published(), forge.opened()),
        (
            OpenRefusal::UnusableTitle {
                title: "--- !!! ---".to_string(),
            },
            String::new(),
            String::new()
        )
    );
}

#[tokio::test]
async fn a_dirty_working_tree_is_refused_before_publishing() {
    let (_base, state) = review(Some("Refactor the widget"));
    let forge = FakeForge::new();
    let mut repo = FakeRepo::ready();
    repo.clean = false;
    let (url, commit) = (repo_url(), head());

    let refusal = open_pull_request(&forge, &repo, &state, &request(&url, &commit))
        .await
        .expect_err("a dirty tree is refused")
        .downcast::<OpenRefusal>()
        .expect("an OpenRefusal");

    wince::assert_eq!(
        (refusal, repo.published(), forge.opened()),
        (OpenRefusal::DirtyWorkingTree, String::new(), String::new())
    );
}

#[tokio::test]
async fn an_existing_upstream_is_refused_before_publishing() {
    let (_base, state) = review(Some("Refactor the widget"));
    let forge = FakeForge::new();
    let mut repo = FakeRepo::ready();
    repo.upstream = Some(TrackingBranch {
        remote: "origin".to_string(),
        branch: "main".to_string(),
    });
    let (url, commit) = (repo_url(), head());

    let refusal = open_pull_request(&forge, &repo, &state, &request(&url, &commit))
        .await
        .expect_err("an existing upstream is refused")
        .downcast::<OpenRefusal>()
        .expect("an OpenRefusal");

    wince::assert_eq!(
        (refusal, repo.published(), forge.opened()),
        (
            OpenRefusal::UpstreamAlreadySet {
                remote: "origin".to_string(),
                branch: "main".to_string(),
            },
            String::new(),
            String::new()
        )
    );
}
