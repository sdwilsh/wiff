#![allow(missing_docs)]

use time::OffsetDateTime;
use time::macros::datetime;
use ulid::Ulid;
use wiff_core::comment::{disposition_event, edit_event, link_event, resolve_event};
use wiff_core::record::{
    Author, AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentTarget, Description,
    DescriptionRecord, DiffVersionRecord, Disposition, ExternalKind, ExternalRef, FORMAT_VERSION,
    ForgeId, Record, RecordBody, RevisionId, ScmSource, Seq, SessionHeader, SourceKind, TipRule,
    VerdictSyncRecord, VersionNumber, comment_body_marker,
};
use wiff_core::review::{ReviewState, fold};
use wiff_core::{BaseRuleset, ScmType};
use wiff_diff::{LineNo, Side};
use wiff_forge::{PushPlan, plan_push};

/// The head commit every diff version in these tests was captured at, the commit
/// an inline comment's forge anchor is stamped with.
const HEAD: &str = "cafe";

fn human(name: &str) -> Author {
    Author {
        name: name.to_string(),
        kind: AuthorKind::Human,
    }
}

fn agent(name: &str) -> Author {
    Author {
        name: name.to_string(),
        kind: AuthorKind::Agent,
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

/// A locally authored comment `id` by `author` with no forge object yet, the
/// kind push publishes and then links.
fn create(id: u128, author: &str, target: CommentTarget, body: &str) -> RecordBody {
    create_with(id, author, target, body, None)
}

fn create_with(
    id: u128,
    author: &str,
    target: CommentTarget,
    body: &str,
    disposition: Option<Disposition>,
) -> RecordBody {
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
            disposition,
        }),
    })
}

fn header() -> SessionHeader {
    SessionHeader {
        ulid: Ulid::from_string("00000000000000000000000009").unwrap(),
        version: FORMAT_VERSION,
        project: "demo".to_string(),
        repo_root: Some("/repos/demo".to_string()),
        cwd: "/repos/demo".to_string(),
        source: SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("ref(name(deadbeef))"),
            tip: TipRule::WorkingCopy,
            branch_hint: None,
        }),
        forge: None,
    }
}

/// Version 0 captured at [`HEAD`], the commit an inline comment anchors against.
fn version() -> DiffVersionRecord {
    DiffVersionRecord {
        number: VersionNumber(0),
        diff_hash: wiff_core::SidebandHash::of(b"f.txt"),
        base_revision: None,
        base_tip_relative: false,
        head_revision: Some(RevisionId(HEAD.to_string())),
        files: Vec::new(),
    }
}

/// Fold `events` into a review, ahead of them a session header and a captured
/// version 0 so an inline comment has a head commit to anchor against.
fn fold_state(events: Vec<RecordBody>) -> ReviewState {
    let mut records = vec![
        Record {
            seq: Seq(0),
            at: OffsetDateTime::UNIX_EPOCH,
            body: RecordBody::Session(header()),
        },
        Record {
            seq: Seq(1),
            at: OffsetDateTime::UNIX_EPOCH,
            body: RecordBody::DiffVersion(version()),
        },
    ];
    for (offset, body) in events.into_iter().enumerate() {
        records.push(Record {
            seq: Seq(offset as u64 + 2),
            at: datetime!(2024-06-01 09:00 UTC),
            body,
        });
    }
    fold(&records).expect("fold review events")
}

fn plan(events: Vec<RecordBody>) -> PushPlan {
    plan_push(&fold_state(events), &human("wez")).expect("a plan without refused comments")
}

#[test]
fn a_fresh_review_batches_inline_comments_and_submits_the_verdict() {
    // wez's own fresh work: an inline comment requesting changes, a review-level
    // comment, a reply to a comment already on the forge, and a reply to the
    // still-unlinked inline comment. Only the inline comment anchors into the
    // batch; the two postable comments stand alone; the reply whose parent is
    // not yet linked waits.
    let linked = {
        let create = create(0x10, "wez", CommentTarget::Review, "already on the forge");
        let link = link_event(
            Ulid::from(0x10u128),
            human("wez"),
            origin("900"),
            comment_body_marker("already on the forge"),
        );
        vec![create, link]
    };
    let mut events = linked;
    events.push(create_with(
        1,
        "wez",
        lines("f.txt", 11),
        "needs a test",
        Some(Disposition::RequestChanges),
    ));
    events.push(create(
        2,
        "wez",
        CommentTarget::Review,
        "overall looks fine",
    ));
    events.push(create(
        3,
        "wez",
        CommentTarget::Comment {
            id: Ulid::from(0x10u128),
        },
        "reply to the linked one",
    ));
    events.push(create(
        4,
        "wez",
        CommentTarget::Comment {
            id: Ulid::from(1u128),
        },
        "reply to the unlinked inline",
    ));

    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{:#?}", plan(events)),
        r#"PushPlan {
    review: Some(
        OutgoingReview {
            disposition: Some(
                RequestChanges,
            ),
            body: "",
            comments: [
                OutgoingComment {
                    comment: Ulid(
                        1,
                    ),
                    body: "needs a test",
                    disposition: Some(
                        RequestChanges,
                    ),
                    anchor: Some(
                        ForgeAnchor {
                            path: "f.txt",
                            side: After,
                            start_line: LineNo(
                                11,
                            ),
                            end_line: LineNo(
                                11,
                            ),
                            commit: RevisionId(
                                "cafe",
                            ),
                        },
                    ),
                    reply_to: None,
                },
            ],
        },
    ),
    posts: [
        OutgoingComment {
            comment: Ulid(
                2,
            ),
            body: "overall looks fine",
            disposition: None,
            anchor: None,
            reply_to: None,
        },
        OutgoingComment {
            comment: Ulid(
                3,
            ),
            body: "reply to the linked one",
            disposition: None,
            anchor: None,
            reply_to: Some(
                ExternalRef {
                    forge: ForgeId {
                        provider: "github",
                        host: "github.com",
                    },
                    kind: ReviewComment,
                    id: "900",
                    url: None,
                },
            ),
        },
    ],
    edits: [],
    resolves: [],
    description: None,
}"#,
    );
}

#[test]
fn only_the_pushers_own_human_comments_are_published() {
    // A comment by another human and an agent comment sharing wez's name are
    // both left for their own authors; neither is wez's to publish.
    let events = vec![
        create(1, "alice", lines("f.txt", 11), "not wez's to send"),
        RecordBody::CommentEvent(CommentEvent {
            id: Ulid::from(2u128),
            author: agent("wez"),
            authored_at: None,
            origin: None,
            kind: CommentEventKind::Create(CommentCreate {
                target: CommentTarget::Review,
                version: VersionNumber(0),
                anchor: None,
                body: "an agent's note".to_string(),
                disposition: None,
            }),
        }),
    ];

    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{:#?}", plan(events)),
        "PushPlan {\n",
        "    review: None,\n",
        "    posts: [],\n",
        "    edits: [],\n",
        "    resolves: [],\n",
        "    description: None,\n",
        "}",
    );
}

#[test]
fn a_linked_comment_edited_and_resolved_locally_plans_an_edit_and_a_resolve() {
    // Link a comment recording the forge's body, then edit and resolve it
    // locally without pushing. The marker no longer matches either change, so
    // both are planned against the recorded forge object.
    let events = vec![
        create(1, "wez", lines("f.txt", 11), "first wording"),
        link_event(
            Ulid::from(1u128),
            human("wez"),
            origin("901"),
            comment_body_marker("first wording"),
        ),
        edit_event(
            Ulid::from(1u128),
            human("wez"),
            "sharper wording".to_string(),
        ),
        resolve_event(Ulid::from(1u128), human("wez"), true),
    ];

    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{:#?}", plan(events)),
        r#"PushPlan {
    review: None,
    posts: [],
    edits: [
        PushEdit {
            comment: Ulid(
                1,
            ),
            at: ExternalRef {
                forge: ForgeId {
                    provider: "github",
                    host: "github.com",
                },
                kind: ReviewComment,
                id: "901",
                url: None,
            },
            body: "sharper wording",
        },
    ],
    resolves: [
        PushResolve {
            comment: Ulid(
                1,
            ),
            at: ExternalRef {
                forge: ForgeId {
                    provider: "github",
                    host: "github.com",
                },
                kind: ReviewComment,
                id: "901",
                url: None,
            },
            resolved: true,
        },
    ],
    description: None,
}"#,
    );
}

#[test]
fn a_linked_comment_matching_its_marker_plans_nothing() {
    // A linked comment untouched since its last sync: body and resolution match
    // the marker, so there is nothing to push.
    let events = vec![
        create(1, "wez", lines("f.txt", 11), "unchanged"),
        link_event(
            Ulid::from(1u128),
            human("wez"),
            origin("902"),
            comment_body_marker("unchanged"),
        ),
    ];
    let plan = plan(events);
    wince::assert_eq!(plan.is_empty(), true);
    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{plan:#?}"),
        "PushPlan {\n",
        "    review: None,\n",
        "    posts: [],\n",
        "    edits: [],\n",
        "    resolves: [],\n",
        "    description: None,\n",
        "}",
    );
}

#[test]
fn a_locally_set_description_is_planned() {
    // A description set locally has no synced marker, so it differs from the
    // forge and is planned for publication.
    let events = vec![RecordBody::Description(DescriptionRecord {
        author: human("wez"),
        authored_at: None,
        origin: None,
        synced_marker: None,
        description: Description {
            title: "Add the widget".to_string(),
            body: "It does the thing.".to_string(),
        },
    })];

    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{:#?}", plan(events)),
        r#"PushPlan {
    review: None,
    posts: [],
    edits: [],
    resolves: [],
    description: Some(
        Description {
            title: "Add the widget",
            body: "It does the thing.",
        },
    ),
}"#,
    );
}

#[test]
fn a_description_matching_its_synced_marker_plans_nothing() {
    // A description whose synced marker fingerprints its current content is in
    // step with the forge, so there is nothing to publish.
    let content = Description {
        title: "Add the widget".to_string(),
        body: "It does the thing.".to_string(),
    };
    let events = vec![RecordBody::Description(DescriptionRecord {
        author: human("wez"),
        authored_at: None,
        origin: None,
        synced_marker: Some(content.content_marker()),
        description: content,
    })];
    let plan = plan(events);
    wince::assert_eq!(plan.is_empty(), true);
    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{plan:#?}"),
        "PushPlan {\n",
        "    review: None,\n",
        "    posts: [],\n",
        "    edits: [],\n",
        "    resolves: [],\n",
        "    description: None,\n",
        "}",
    );
}

#[test]
fn a_verdict_never_pushed_submits_a_review_with_no_fresh_inline_comments() {
    // wez's only verdict is on an already-linked comment, so there is no fresh
    // inline comment to batch. With no pushed-verdict marker the verdict counts
    // as unsent, so a review submits on its own with the disposition and no
    // comments.
    let events = vec![
        create_with(
            1,
            "wez",
            CommentTarget::Review,
            "looks good",
            Some(Disposition::Approve),
        ),
        link_event(
            Ulid::from(1u128),
            human("wez"),
            origin("903"),
            comment_body_marker("looks good"),
        ),
    ];

    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{:#?}", plan(events)),
        r#"PushPlan {
    review: Some(
        OutgoingReview {
            disposition: Some(
                Approve,
            ),
            body: "",
            comments: [],
        },
    ),
    posts: [],
    edits: [],
    resolves: [],
    description: None,
}"#,
    );
}

#[test]
fn a_verdict_matching_its_pushed_marker_plans_nothing() {
    // The same approve, now recorded as already pushed. Current and last-pushed
    // agree, and there is no other work, so the plan is empty.
    let events = vec![
        create_with(
            1,
            "wez",
            CommentTarget::Review,
            "looks good",
            Some(Disposition::Approve),
        ),
        link_event(
            Ulid::from(1u128),
            human("wez"),
            origin("903"),
            comment_body_marker("looks good"),
        ),
        RecordBody::VerdictSync(VerdictSyncRecord {
            author: human("wez"),
            disposition: Disposition::Approve,
        }),
    ];
    let plan = plan(events);
    wince::assert_eq!(plan.is_empty(), true);
    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{plan:#?}"),
        "PushPlan {\n",
        "    review: None,\n",
        "    posts: [],\n",
        "    edits: [],\n",
        "    resolves: [],\n",
        "    description: None,\n",
        "}",
    );
}

#[test]
fn a_verdict_changed_since_it_was_pushed_replans_a_review_with_the_new_disposition() {
    // wez pushed an approve, then changed their mind to request changes. The
    // current verdict differs from the pushed one, so a review re-plans with the
    // new disposition and no fresh inline comments (the comment is already
    // linked).
    let events = vec![
        create_with(
            1,
            "wez",
            CommentTarget::Review,
            "looks good",
            Some(Disposition::Approve),
        ),
        link_event(
            Ulid::from(1u128),
            human("wez"),
            origin("903"),
            comment_body_marker("looks good"),
        ),
        RecordBody::VerdictSync(VerdictSyncRecord {
            author: human("wez"),
            disposition: Disposition::Approve,
        }),
        disposition_event(
            Ulid::from(1u128),
            human("wez"),
            Some(Disposition::RequestChanges),
        ),
    ];

    #[rustfmt::skip]
    wince::snapshot_str!(
        format!("{:#?}", plan(events)),
        r#"PushPlan {
    review: Some(
        OutgoingReview {
            disposition: Some(
                RequestChanges,
            ),
            body: "",
            comments: [],
        },
    ),
    posts: [],
    edits: [],
    resolves: [],
    description: None,
}"#,
    );
}

#[test]
fn a_line_comment_on_an_uncommitted_version_is_refused() {
    // Version 0 here is captured from uncommitted work: it has no head commit.
    // A line comment against it is refused by name rather than degraded to a
    // locationless review-level post.
    let headless = DiffVersionRecord {
        number: VersionNumber(0),
        diff_hash: wiff_core::SidebandHash::of(b"f.txt"),
        base_revision: None,
        base_tip_relative: false,
        head_revision: None,
        files: Vec::new(),
    };
    let records = vec![
        Record {
            seq: Seq(0),
            at: OffsetDateTime::UNIX_EPOCH,
            body: RecordBody::Session(header()),
        },
        Record {
            seq: Seq(1),
            at: OffsetDateTime::UNIX_EPOCH,
            body: RecordBody::DiffVersion(headless),
        },
        Record {
            seq: Seq(2),
            at: datetime!(2024-06-01 09:00 UTC),
            body: create(1, "wez", lines("f.txt", 3), "off-by-one here"),
        },
    ];
    let state = fold(&records).expect("fold review events");

    let error = plan_push(&state, &human("wez")).unwrap_err();
    wince::snapshot_display!(
        error,
        "comment #1 is on lines of a diff captured from uncommitted work, which the forge has no commit for; commit the reviewed changes, then push"
    );
}
