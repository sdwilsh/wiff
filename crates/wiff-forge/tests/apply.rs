#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use ulid::Ulid;
use wiff_core::comment::{edit_event, link_event, resolve_event};
use wiff_core::record::{
    Author, AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentTarget, Description,
    DescriptionRecord, DiffVersionRecord, Disposition, ExternalKind, ExternalRef, FORMAT_VERSION,
    ForgeId, ForgeUrl, RecordBody, RevisionId, ScmSource, SessionHeader, SourceKind, TipRule,
    VersionNumber, comment_body_marker,
};
use wiff_core::review::ReviewState;
use wiff_core::{BaseRuleset, ScmType, SessionLog, SidebandHash};
use wiff_diff::{LineNo, Side};
use wiff_forge::{
    DeclinedWrite, Forge, NewPullRequest, OutgoingComment, OutgoingReview, PushOutcome,
    SubmittedReview, Unsupported, push,
};

/// The head commit version 0 was captured at, the commit an inline comment's
/// forge anchor is stamped with.
const HEAD: &str = "cafe";

fn human(name: &str) -> Author {
    Author {
        name: name.to_string(),
        kind: AuthorKind::Human,
    }
}

/// A forge object reference numbered `id`, all on one fake GitHub instance.
fn origin(id: &str) -> ExternalRef {
    ExternalRef {
        forge: ForgeId {
            provider: "github".to_string(),
            host: "github.com".to_string(),
        },
        kind: ExternalKind::ReviewComment,
        id: id.to_string(),
        url: None,
    }
}

fn lines(file: &str, line: u32) -> CommentTarget {
    CommentTarget::Lines {
        file: file.to_string(),
        side: Side::After,
        start_line: LineNo::new(line).unwrap(),
        end_line: LineNo::new(line).unwrap(),
    }
}

/// A locally authored comment `id` by `author` with no forge object yet.
fn create(id: u128, author: &str, target: CommentTarget, body: &str) -> RecordBody {
    RecordBody::CommentEvent(CommentEvent {
        id: Ulid::from(id),
        author: human(author),
        authored_at: None,
        origin: None,
        kind: CommentEventKind::Create(CommentCreate {
            target,
            version: VersionNumber(0),
            anchor: None,
            body: body.to_string(),
            disposition: None,
        }),
    })
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

/// Version 0 captured at [`HEAD`], the commit an inline comment anchors against.
fn version() -> DiffVersionRecord {
    DiffVersionRecord {
        number: VersionNumber(0),
        diff_hash: SidebandHash::of(b"f.txt"),
        base_revision: None,
        base_tip_relative: false,
        head_revision: Some(RevisionId(HEAD.to_string())),
        files: Vec::new(),
    }
}

fn pull_request() -> ForgeUrl {
    ForgeUrl::parse("https://github.com/o/r/pull/1").expect("valid url")
}

/// Build a real session file whose log is the header, version 0, then `events`,
/// returning it open for appending.
fn session_with(events: Vec<RecordBody>) -> (tempfile::TempDir, SessionLog) {
    let base = tempfile::tempdir().expect("tempdir");
    let (mut log, mut lock) = SessionLog::create(base.path(), "demo", |ulid| {
        RecordBody::Session(header(ulid))
    })
    .expect("create session");
    log.append(&mut lock, RecordBody::DiffVersion(version()))
        .expect("append version");
    for body in events {
        log.append(&mut lock, body).expect("append event");
    }
    drop(lock);
    (base, log)
}

/// The comment events a push appended, folded back and rendered per comment as
/// its origin, resolution, and whether each synced marker agrees with the local
/// state, plus each actor's pushed verdict and whether the description agrees
/// with its synced marker.
fn published(log: &SessionLog) -> String {
    let state = ReviewState::load(log.path()).expect("fold");
    let mut out = String::new();
    for comment in &state.comments {
        let origin = comment
            .origin
            .as_ref()
            .map(|o| o.id.as_str())
            .unwrap_or("-");
        let synced = match &comment.synced {
            Some(synced) => format!(
                "synced_body={} synced_resolved={}",
                synced.body_marker == comment_body_marker(&comment.body),
                synced.resolved
            ),
            None => "unsynced".to_string(),
        };
        out.push_str(&format!(
            "{} origin={origin} resolved={} {synced}\n",
            comment.id, comment.resolved
        ));
    }
    for verdict in &state.pushed_verdicts {
        out.push_str(&format!(
            "verdict {} = {:?}\n",
            verdict.author.name, verdict.disposition
        ));
    }
    if let Some(description) = &state.description {
        let synced = description.synced_marker.as_deref()
            == Some(description.content.content_marker().as_str());
        out.push_str(&format!("description synced={synced}\n"));
    }
    out
}

/// A forge that records every call and hands back predictable object refs. It
/// can be told to decline resolution or description writes, standing in for a
/// forge that does not implement them.
struct FakeForge {
    calls: Mutex<Vec<String>>,
    resolve_supported: bool,
    description_supported: bool,
    /// Whether a declined write returns its `Unsupported` wrapped in an
    /// `anyhow` context, as a real adapter would when it annotates the failure,
    /// rather than the bare error.
    wrap_declines: bool,
}

impl FakeForge {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            resolve_supported: true,
            description_supported: true,
            wrap_declines: false,
        }
    }

    /// The `Unsupported` error a declined write returns, wrapped in adapter
    /// context when `wrap_declines` is set.
    fn declined(&self) -> anyhow::Error {
        if self.wrap_declines {
            anyhow::Error::from(Unsupported).context("github: resolving review thread")
        } else {
            Unsupported.into()
        }
    }

    fn log(&self, call: String) {
        self.calls.lock().expect("lock").push(call);
    }

    fn calls(&self) -> String {
        self.calls.lock().expect("lock").join("\n")
    }
}

#[async_trait]
impl Forge for FakeForge {
    async fn fetch(&self, _pr: &ForgeUrl) -> Result<wiff_forge::FetchedPullRequest> {
        unreachable!("push does not fetch")
    }

    async fn submit_review(
        &self,
        _pr: &ForgeUrl,
        review: &OutgoingReview,
    ) -> Result<SubmittedReview> {
        let ids: Vec<String> = review
            .comments
            .iter()
            .map(|c| c.comment.to_string())
            .collect();
        self.log(format!(
            "submit_review disposition={:?} comments=[{}]",
            review.disposition,
            ids.join(", ")
        ));
        let comments: BTreeMap<Ulid, ExternalRef> = review
            .comments
            .iter()
            .map(|c| (c.comment, origin(&format!("fc-{}", c.comment))))
            .collect();
        Ok(SubmittedReview {
            review: origin("review"),
            comments,
        })
    }

    async fn post_comment(&self, _pr: &ForgeUrl, comment: &OutgoingComment) -> Result<ExternalRef> {
        self.log(format!(
            "post_comment comment={} reply_to={:?} inline={}",
            comment.comment,
            comment.reply_to.as_ref().map(|r| r.id.clone()),
            comment.anchor.is_some()
        ));
        Ok(origin(&format!("pc-{}", comment.comment)))
    }

    async fn edit_comment(&self, at: &ExternalRef, body: &str) -> Result<()> {
        self.log(format!("edit_comment at={} body={body:?}", at.id));
        Ok(())
    }

    async fn set_resolved(&self, at: &ExternalRef, resolved: bool) -> Result<()> {
        self.log(format!("set_resolved at={} resolved={resolved}", at.id));
        if self.resolve_supported {
            Ok(())
        } else {
            Err(self.declined())
        }
    }

    async fn set_description(&self, _pr: &ForgeUrl, description: &Description) -> Result<()> {
        self.log(format!("set_description title={:?}", description.title));
        if self.description_supported {
            Ok(())
        } else {
            Err(self.declined())
        }
    }

    async fn create_pull_request(&self, _req: &NewPullRequest) -> Result<ForgeUrl> {
        unreachable!("push does not open a pull request")
    }
}

/// A linked comment `id` by `author`: a create followed by the link push would
/// have appended, so it already mirrors forge object `forge_id` with `body` as
/// its synced marker.
fn linked(
    id: u128,
    author: &str,
    target: CommentTarget,
    body: &str,
    forge_id: &str,
) -> Vec<RecordBody> {
    vec![
        create(id, author, target, body),
        link_event(
            Ulid::from(id),
            human(author),
            origin(forge_id),
            comment_body_marker(body),
        ),
    ]
}

#[tokio::test]
async fn a_fresh_review_submits_inline_and_verdict_and_posts_a_fallback() {
    // wez's own fresh work: an inline comment that anchors, a review-level
    // comment that posts standalone, and a request-changes verdict. The inline
    // comment and verdict go up as one batched review; the review-level comment
    // follows as a standalone post. Both comments end linked, the verdict
    // recorded as pushed.
    let (_base, mut log) = session_with(vec![
        RecordBody::CommentEvent(CommentEvent {
            id: Ulid::from(1u128),
            author: human("wez"),
            authored_at: None,
            origin: None,
            kind: CommentEventKind::Create(CommentCreate {
                target: lines("f.txt", 3),
                version: VersionNumber(0),
                anchor: None,
                body: "off-by-one here".to_string(),
                disposition: Some(Disposition::RequestChanges),
            }),
        }),
        create(2, "wez", CommentTarget::Review, "overall looks close"),
    ]);
    let forge = FakeForge::new();

    let outcome = push(&forge, &mut log, &pull_request(), &human("wez"))
        .await
        .expect("push succeeds");

    wince::assert_eq!(
        outcome,
        PushOutcome {
            created: vec![Ulid::from(1u128), Ulid::from(2u128)],
            edited: vec![],
            resolved: vec![],
            verdict_submitted: true,
            description_published: false,
            declined: vec![],
        }
    );
    wince::snapshot_str!(
        forge.calls(),
        "submit_review disposition=Some(RequestChanges) comments=[00000000000000000000000001]\npost_comment comment=00000000000000000000000002 reply_to=None inline=false"
    );
    #[rustfmt::skip]
    wince::snapshot_str!(
        published(&log),
        "00000000000000000000000001 origin=fc-00000000000000000000000001 resolved=false synced_body=true synced_resolved=false\n",
        "00000000000000000000000002 origin=pc-00000000000000000000000002 resolved=false synced_body=true synced_resolved=false\n",
        "verdict wez = RequestChanges\n",
    );
}

#[tokio::test]
async fn a_reply_waits_for_its_parents_link() {
    // A local root comment and a local reply to it. The reply cannot be posted
    // until the root has a forge object, so the first pass posts the root and
    // the re-plan posts the reply against the root's fresh object.
    let (_base, mut log) = session_with(vec![
        create(1, "wez", CommentTarget::Review, "the root"),
        RecordBody::CommentEvent(CommentEvent {
            id: Ulid::from(2u128),
            author: human("wez"),
            authored_at: None,
            origin: None,
            kind: CommentEventKind::Create(CommentCreate {
                target: CommentTarget::Comment {
                    id: Ulid::from(1u128),
                },
                version: VersionNumber(0),
                anchor: None,
                body: "a follow-up".to_string(),
                disposition: None,
            }),
        }),
    ]);
    let forge = FakeForge::new();

    let outcome = push(&forge, &mut log, &pull_request(), &human("wez"))
        .await
        .expect("push succeeds");

    wince::assert_eq!(
        outcome,
        PushOutcome {
            created: vec![Ulid::from(1u128), Ulid::from(2u128)],
            edited: vec![],
            resolved: vec![],
            verdict_submitted: false,
            description_published: false,
            declined: vec![],
        }
    );
    wince::snapshot_str!(
        forge.calls(),
        "post_comment comment=00000000000000000000000001 reply_to=None inline=false\npost_comment comment=00000000000000000000000002 reply_to=Some(\"pc-00000000000000000000000001\") inline=false"
    );
    #[rustfmt::skip]
    wince::snapshot_str!(
        published(&log),
        "00000000000000000000000001 origin=pc-00000000000000000000000001 resolved=false synced_body=true synced_resolved=false\n",
        "00000000000000000000000002 origin=pc-00000000000000000000000002 resolved=false synced_body=true synced_resolved=false\n",
    );
}

#[tokio::test]
async fn an_unpushed_edit_and_resolve_advance_their_markers() {
    // A linked comment with an unpushed body edit and an unpushed resolve.
    // Push sends each and advances the matching marker, so a second push finds
    // nothing to do.
    let mut events = linked(1, "wez", lines("f.txt", 3), "first take", "gh-1");
    events.push(edit_event(
        Ulid::from(1u128),
        human("wez"),
        "sharper take".to_string(),
    ));
    events.push(resolve_event(Ulid::from(1u128), human("wez"), true));
    let (_base, mut log) = session_with(events);
    let forge = FakeForge::new();

    let outcome = push(&forge, &mut log, &pull_request(), &human("wez"))
        .await
        .expect("push succeeds");

    wince::assert_eq!(
        outcome,
        PushOutcome {
            created: vec![],
            edited: vec![Ulid::from(1u128)],
            resolved: vec![Ulid::from(1u128)],
            verdict_submitted: false,
            description_published: false,
            declined: vec![],
        }
    );
    wince::snapshot_str!(
        forge.calls(),
        "edit_comment at=gh-1 body=\"sharper take\"\nset_resolved at=gh-1 resolved=true"
    );

    let again = push(&forge, &mut log, &pull_request(), &human("wez"))
        .await
        .expect("second push succeeds");
    wince::assert_eq!(again, PushOutcome::default());
}

#[tokio::test]
async fn an_unpushed_description_edit_is_published_and_its_marker_advanced() {
    // The description was pulled once (its marker records the old upstream text)
    // then edited locally. Push sends the new title and body and advances the
    // marker to the published content, so a second push finds nothing to do.
    let old = Description {
        title: "Old title".to_string(),
        body: "old body".to_string(),
    };
    let new = Description {
        title: "New title".to_string(),
        body: "new body".to_string(),
    };
    let (_base, mut log) = session_with(vec![
        RecordBody::Description(DescriptionRecord {
            author: human("wez"),
            authored_at: None,
            origin: Some(origin("pr-body")),
            synced_marker: Some(old.content_marker()),
            description: old,
        }),
        RecordBody::Description(DescriptionRecord {
            author: human("wez"),
            authored_at: None,
            origin: None,
            synced_marker: None,
            description: new,
        }),
    ]);
    let forge = FakeForge::new();

    let outcome = push(&forge, &mut log, &pull_request(), &human("wez"))
        .await
        .expect("push succeeds");

    wince::assert_eq!(
        outcome,
        PushOutcome {
            created: vec![],
            edited: vec![],
            resolved: vec![],
            verdict_submitted: false,
            description_published: true,
            declined: vec![],
        }
    );
    wince::snapshot_str!(forge.calls(), "set_description title=\"New title\"");
    wince::snapshot_str!(published(&log), "description synced=true\n");

    let again = push(&forge, &mut log, &pull_request(), &human("wez"))
        .await
        .expect("second push succeeds");
    wince::assert_eq!(again, PushOutcome::default());
}

#[tokio::test]
async fn a_forge_that_cannot_resolve_keeps_the_local_resolution() {
    // The forge declines set_resolved. Push reports it declined and, crucially,
    // does not advance the comment's marker, so the loop still settles and a
    // later pull will not read the marker as agreeing with an unresolved forge
    // and revert the local resolution.
    let mut events = linked(1, "wez", lines("f.txt", 3), "a note", "gh-1");
    events.push(resolve_event(Ulid::from(1u128), human("wez"), true));
    let (_base, mut log) = session_with(events);
    let forge = FakeForge {
        resolve_supported: false,
        ..FakeForge::new()
    };

    let outcome = push(&forge, &mut log, &pull_request(), &human("wez"))
        .await
        .expect("push succeeds");

    wince::assert_eq!(
        outcome,
        PushOutcome {
            created: vec![],
            edited: vec![],
            resolved: vec![],
            verdict_submitted: false,
            description_published: false,
            declined: vec![DeclinedWrite::Resolution(Ulid::from(1u128))],
        }
    );
    wince::snapshot_str!(forge.calls(), "set_resolved at=gh-1 resolved=true");
    // The marker stays unresolved (from the link) while the comment is resolved
    // locally: the local intent is intact for a later push to retry.
    wince::snapshot_str!(
        published(&log),
        "00000000000000000000000001 origin=gh-1 resolved=true synced_body=true synced_resolved=false\n"
    );
}

#[tokio::test]
async fn a_declined_write_is_recognized_through_adapter_context() {
    // A real adapter annotates its failures, so the Unsupported error arrives
    // wrapped in context rather than bare. Push must still classify it as a
    // decline through the anyhow chain, not treat the annotated error as a hard
    // failure. Same fixture as the bare-decline test, with the wrapping on.
    let mut events = linked(1, "wez", lines("f.txt", 3), "a note", "gh-1");
    events.push(resolve_event(Ulid::from(1u128), human("wez"), true));
    let (_base, mut log) = session_with(events);
    let forge = FakeForge {
        resolve_supported: false,
        wrap_declines: true,
        ..FakeForge::new()
    };

    let outcome = push(&forge, &mut log, &pull_request(), &human("wez"))
        .await
        .expect("push succeeds");

    wince::assert_eq!(
        outcome,
        PushOutcome {
            created: vec![],
            edited: vec![],
            resolved: vec![],
            verdict_submitted: false,
            description_published: false,
            declined: vec![DeclinedWrite::Resolution(Ulid::from(1u128))],
        }
    );
    wince::snapshot_str!(forge.calls(), "set_resolved at=gh-1 resolved=true");
    wince::snapshot_str!(
        published(&log),
        "00000000000000000000000001 origin=gh-1 resolved=true synced_body=true synced_resolved=false\n"
    );
}
