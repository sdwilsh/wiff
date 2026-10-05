#![allow(missing_docs)]

use time::OffsetDateTime;
use time::macros::datetime;
use ulid::Ulid;
use wiff_core::comment::{edit_event, link_event, resolve_event};
use wiff_core::record::{
    Author, AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentTarget, Description,
    DescriptionRecord, Disposition, ExternalKind, ExternalRef, FORMAT_VERSION, ForgeId, Record,
    RecordBody, RevisionId, ScmSource, Seq, SessionHeader, SourceKind, TipRule, VersionNumber,
    comment_body_marker,
};
use wiff_core::review::{CommentState, DescriptionState, ReviewState, fold};
use wiff_core::{BaseRuleset, ScmType};
use wiff_diff::parse::parse;
use wiff_diff::{Diff, LineNo, Side};
use wiff_forge::types::{
    FetchedComment, FetchedDescription, FetchedReview, ForgeAnchor, Resolution,
};
use wiff_forge::{reconcile_comments, reconcile_description, reconcile_reviews};

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

fn diff() -> Diff {
    parse(DIFF).expect("parse diff")
}

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

/// A monotonic id source, so a reconcile's freshly imported comments take stable
/// ids in the order they are created.
fn ids() -> impl FnMut() -> Ulid {
    let mut next: u128 = 1;
    move || {
        let id = Ulid::from(next);
        next += 1;
        id
    }
}

fn inline(path: &str, side: Side, line: u32) -> ForgeAnchor {
    ForgeAnchor {
        path: path.to_string(),
        side,
        start_line: LineNo::new(line).unwrap(),
        end_line: LineNo::new(line).unwrap(),
        commit: RevisionId("cafe".to_string()),
    }
}

/// A locally authored review-level comment `id` by `author`, the kind push
/// later links to the forge object it creates.
fn local_comment(id: u128, author: &str, body: &str) -> RecordBody {
    RecordBody::CommentEvent(CommentEvent {
        id: Ulid::from(id),
        author: human(author),
        authored_at: None,
        origin: None,
        kind: CommentEventKind::Create(CommentCreate {
            target: CommentTarget::Review,
            version: VersionNumber(0),
            anchor: None,
            body: body.to_string(),
            disposition: None,
        }),
    })
}

fn comment(id: &str, author: &str, body: &str) -> FetchedComment {
    FetchedComment {
        origin: origin(id),
        author: human(author),
        body: body.to_string(),
        authored_at: datetime!(2024-03-01 12:00 UTC),
        anchor: None,
        reply_to: None,
        resolution: None,
    }
}

/// A verdict-submission reference numbered `id`, distinct from a review
/// comment's reference so the two reconcile independently.
fn verdict_origin(id: &str) -> ExternalRef {
    ExternalRef {
        kind: ExternalKind::Verdict,
        ..origin(id)
    }
}

fn review(
    id: &str,
    author: &str,
    body: &str,
    disposition: Option<Disposition>,
    dismissed: bool,
) -> FetchedReview {
    FetchedReview {
        origin: verdict_origin(id),
        author: human(author),
        body: body.to_string(),
        authored_at: datetime!(2024-03-01 12:00 UTC),
        disposition,
        dismissed,
    }
}

/// Fold `events` into a review, prepending a session header so the log is
/// well-formed, and return the reconciled comments.
fn fold_events(events: Vec<RecordBody>) -> Vec<CommentState> {
    let mut records = vec![Record {
        seq: Seq(0),
        at: OffsetDateTime::UNIX_EPOCH,
        body: RecordBody::Session(header()),
    }];
    for (offset, body) in events.into_iter().enumerate() {
        records.push(Record {
            seq: Seq(offset as u64 + 1),
            at: datetime!(2024-06-01 09:00 UTC),
            body,
        });
    }
    fold(&records).expect("fold reconciled events").comments
}

/// Fold `events` into a review as [`fold_events`] does, returning the comments
/// alongside the verdicts derived from them.
fn fold_reviews(events: Vec<RecordBody>) -> String {
    let mut records = vec![Record {
        seq: Seq(0),
        at: OffsetDateTime::UNIX_EPOCH,
        body: RecordBody::Session(header()),
    }];
    for (offset, body) in events.into_iter().enumerate() {
        records.push(Record {
            seq: Seq(offset as u64 + 1),
            at: datetime!(2024-06-01 09:00 UTC),
            body,
        });
    }
    let state: ReviewState = fold(&records).expect("fold reconciled events");
    serde_json::to_string_pretty(&serde_json::json!({
        "comments": state.comments,
        "verdicts": state.verdicts,
    }))
    .expect("serialize reconciled review")
}

fn header() -> SessionHeader {
    SessionHeader {
        id: "000000009".parse().unwrap(),
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

fn as_json(comments: &[CommentState]) -> String {
    serde_json::to_string_pretty(comments).expect("serialize comments")
}

/// A fetched pull request whose comments cover every placement: an inline
/// comment that anchors, a review-level comment, a reply threaded onto the
/// first, a resolved thread, and an inline comment whose line is gone from the
/// diff.
fn fetched() -> Vec<FetchedComment> {
    let inline_anchored = FetchedComment {
        anchor: Some(inline("f.txt", Side::After, 11)),
        ..comment("c1", "alice", "anchors to ELEVEN")
    };
    let review_level = comment("c2", "bob", "overall this looks fine");
    let reply = FetchedComment {
        reply_to: Some(origin("c1")),
        ..comment("c3", "bob", "replying to alice")
    };
    let resolved = FetchedComment {
        anchor: Some(inline("f.txt", Side::After, 12)),
        resolution: Some(Resolution {
            by: Some(human("carol")),
        }),
        ..comment("c4", "alice", "resolved thread on twelve")
    };
    let outdated = FetchedComment {
        anchor: Some(inline("f.txt", Side::After, 999)),
        ..comment("c5", "dave", "line no longer in the diff")
    };
    vec![inline_anchored, review_level, reply, resolved, outdated]
}

#[test]
fn a_fresh_pull_imports_every_comment_placement() {
    let events = reconcile_comments(
        &fetched(),
        &[],
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let comments = fold_events(events);
    #[rustfmt::skip]
    wince::snapshot_str!(
        as_json(&comments),
        r#"[
  {
    "id": "00000000000000000000000001",
    "author": {
      "name": "alice",
      "kind": "human"
    },
    "target": {
      "target": "lines",
      "file": "f.txt",
      "side": "after",
      "start_line": 11,
      "end_line": 11
    },
    "version": 0,
    "anchor": {
      "snippet": [
        "ELEVEN"
      ],
      "context_before": [
        "ten"
      ],
      "context_after": [
        "twelve",
        "thirteen"
      ],
      "landmark": {
        "definition": "ELEVEN",
        "relation": "on_definition",
        "offset": 0
      }
    },
    "body": "anchors to ELEVEN",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "alice",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c1"
    },
    "synced": {
      "body_marker": "d75a9870640dfd28f183fc10c0974860b223fd3b0ee31caa3ca25b346a62fbb1",
      "resolved": false
    },
    "number": 1,
    "created_seq": 1,
    "updated_seq": 1
  },
  {
    "id": "00000000000000000000000002",
    "author": {
      "name": "bob",
      "kind": "human"
    },
    "target": {
      "target": "review"
    },
    "version": 0,
    "anchor": null,
    "body": "overall this looks fine",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "bob",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c2"
    },
    "synced": {
      "body_marker": "930f07b2b6f657214b6b0544d98aa6f14dc7dc977410ea41edbf9162c60bf422",
      "resolved": false
    },
    "number": 2,
    "created_seq": 2,
    "updated_seq": 2
  },
  {
    "id": "00000000000000000000000003",
    "author": {
      "name": "bob",
      "kind": "human"
    },
    "target": {
      "target": "comment",
      "id": "00000000000000000000000001"
    },
    "version": 0,
    "anchor": null,
    "body": "replying to alice",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "bob",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c3"
    },
    "synced": {
      "body_marker": "5f903bcb5c42acde7fbb14f34653fa059acd33d1abac9595b7578b1a2a59e643",
      "resolved": false
    },
    "number": 3,
    "created_seq": 3,
    "updated_seq": 3
  },
  {
    "id": "00000000000000000000000004",
    "author": {
      "name": "alice",
      "kind": "human"
    },
    "target": {
      "target": "lines",
      "file": "f.txt",
      "side": "after",
      "start_line": 12,
      "end_line": 12
    },
    "version": 0,
    "anchor": {
      "snippet": [
        "twelve"
      ],
      "context_before": [
        "ten",
        "ELEVEN"
      ],
      "context_after": [
        "thirteen"
      ],
      "landmark": {
        "definition": "twelve",
        "relation": "on_definition",
        "offset": 0
      }
    },
    "body": "resolved thread on twelve",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "carol",
      "kind": "human"
    },
    "resolved": true,
    "resolved_by": {
      "name": "carol",
      "kind": "human"
    },
    "resolved_at": "2024-03-01T12:00:00Z",
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c4"
    },
    "synced": {
      "body_marker": "51fb362af78f2a2904e7ff682a0bfd35216597fe290c6c9fa48f44bc4c4b4984",
      "resolved": true
    },
    "number": 4,
    "created_seq": 4,
    "updated_seq": 5
  },
  {
    "id": "00000000000000000000000005",
    "author": {
      "name": "dave",
      "kind": "human"
    },
    "target": {
      "target": "file",
      "file": "f.txt"
    },
    "version": 0,
    "anchor": null,
    "body": "line no longer in the diff",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "dave",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c5"
    },
    "synced": {
      "body_marker": "2c27a12863176efe1eb7e80b7f76542666e9470ed02f071c00ec155a77dcc5d6",
      "resolved": false
    },
    "number": 5,
    "created_seq": 6,
    "updated_seq": 6
  }
]"#,
    );
}

#[test]
fn re_pulling_an_unchanged_request_appends_nothing() {
    let first = reconcile_comments(
        &fetched(),
        &[],
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let existing = fold_events(first);
    let again = reconcile_comments(
        &fetched(),
        &existing,
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

#[test]
fn a_local_edit_of_a_linked_comment_survives_an_unchanged_pull() {
    let first = reconcile_comments(
        &fetched(),
        &[],
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );

    // Edit and resolve an imported comment locally, without pushing it; the
    // forge still holds the original body and an unresolved thread.
    let imported = fold_events(first.clone());
    let target = imported[0].id;
    let mut local = first;
    local.push(edit_event(
        target,
        human("wez"),
        "sharper local wording".to_string(),
    ));
    local.push(resolve_event(target, human("wez"), true));
    let existing = fold_events(local);

    // Re-pulling the unchanged request must not import the forge's stale body or
    // resolution over the unpushed local change; it leaves the comment for push
    // to send, so reconcile appends nothing.
    let again = reconcile_comments(
        &fetched(),
        &existing,
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

#[test]
fn a_local_resolve_of_a_freshly_linked_comment_survives_an_unchanged_pull() {
    // A locally authored comment, resolved before it was pushed, then linked to
    // the forge object push created for it. Push opens a fresh, unresolved
    // thread, so the local resolution is not yet on the forge.
    let body = "needs a test";
    let id = 1u128;
    let local = vec![
        local_comment(id, "wez", body),
        resolve_event(Ulid::from(id), human("wez"), true),
        link_event(
            Ulid::from(id),
            human("wez"),
            origin("c1"),
            comment_body_marker(body),
        ),
    ];
    let existing = fold_events(local);

    // The forge holds the same body and an unresolved thread. Re-pulling must
    // not import a resolve back to unresolved over the unpushed local one; it
    // leaves the resolution for push to send, so reconcile appends nothing.
    let forge = vec![comment("c1", "wez", body)];
    let again = reconcile_comments(
        &forge,
        &existing,
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

#[test]
fn a_concurrent_forge_edit_wins_over_an_unpushed_local_edit() {
    // Import a review-level comment, then edit it locally without pushing.
    let forge = vec![comment("c2", "bob", "overall this looks fine")];
    let first = reconcile_comments(
        &forge,
        &[],
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let target = fold_events(first.clone())[0].id;
    let local_edit = edit_event(target, human("wez"), "local rewording".to_string());
    let mut existing_events = first.clone();
    existing_events.push(local_edit.clone());
    let existing = fold_events(existing_events);

    // The forge's copy of that comment also changed since the last sync. Both
    // sides diverged from the marker, so the forge wins: its edit imports over
    // the unpushed local one and folding the whole history leaves the comment
    // holding the forge's body.
    let changed = vec![comment("c2", "bob", "on reflection, needs work")];
    let second = reconcile_comments(
        &changed,
        &existing,
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let mut all = first;
    all.push(local_edit);
    all.extend(second);
    let comments = fold_events(all);
    #[rustfmt::skip]
    wince::snapshot_str!(
        as_json(&comments),
        r#"[
  {
    "id": "00000000000000000000000001",
    "author": {
      "name": "bob",
      "kind": "human"
    },
    "target": {
      "target": "review"
    },
    "version": 0,
    "anchor": null,
    "body": "on reflection, needs work",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "bob",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c2"
    },
    "synced": {
      "body_marker": "9b6ea62cc395b2a1764c1f18b3893fc3fa9cf2fd6ea3da3181385c76a3138b2e",
      "resolved": false
    },
    "number": 1,
    "created_seq": 1,
    "updated_seq": 3
  }
]"#,
    );
}

#[test]
fn a_later_pull_edits_resolves_and_withdraws() {
    let first = reconcile_comments(
        &fetched(),
        &[],
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let existing = fold_events(first.clone());

    // The review-level comment's body changed, its thread is now resolved, and
    // the outdated inline comment was deleted upstream.
    let mut later = fetched();
    later[1].body = "on reflection, request changes".to_string();
    later[1].resolution = Some(Resolution {
        by: Some(human("bob")),
    });
    later.pop();

    let second = reconcile_comments(
        &later,
        &existing,
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let mut all = first;
    all.extend(second);
    let comments = fold_events(all);
    #[rustfmt::skip]
    wince::snapshot_str!(
        as_json(&comments),
        r#"[
  {
    "id": "00000000000000000000000001",
    "author": {
      "name": "alice",
      "kind": "human"
    },
    "target": {
      "target": "lines",
      "file": "f.txt",
      "side": "after",
      "start_line": 11,
      "end_line": 11
    },
    "version": 0,
    "anchor": {
      "snippet": [
        "ELEVEN"
      ],
      "context_before": [
        "ten"
      ],
      "context_after": [
        "twelve",
        "thirteen"
      ],
      "landmark": {
        "definition": "ELEVEN",
        "relation": "on_definition",
        "offset": 0
      }
    },
    "body": "anchors to ELEVEN",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "alice",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c1"
    },
    "synced": {
      "body_marker": "d75a9870640dfd28f183fc10c0974860b223fd3b0ee31caa3ca25b346a62fbb1",
      "resolved": false
    },
    "number": 1,
    "created_seq": 1,
    "updated_seq": 1
  },
  {
    "id": "00000000000000000000000002",
    "author": {
      "name": "bob",
      "kind": "human"
    },
    "target": {
      "target": "review"
    },
    "version": 0,
    "anchor": null,
    "body": "on reflection, request changes",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "bob",
      "kind": "human"
    },
    "resolved": true,
    "resolved_by": {
      "name": "bob",
      "kind": "human"
    },
    "resolved_at": "2024-03-01T12:00:00Z",
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c2"
    },
    "synced": {
      "body_marker": "12a42d035cad7d132837fd3fde8dd6181214941741e8ba249530b38785a9016a",
      "resolved": true
    },
    "number": 2,
    "created_seq": 2,
    "updated_seq": 8
  },
  {
    "id": "00000000000000000000000003",
    "author": {
      "name": "bob",
      "kind": "human"
    },
    "target": {
      "target": "comment",
      "id": "00000000000000000000000001"
    },
    "version": 0,
    "anchor": null,
    "body": "replying to alice",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "bob",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c3"
    },
    "synced": {
      "body_marker": "5f903bcb5c42acde7fbb14f34653fa059acd33d1abac9595b7578b1a2a59e643",
      "resolved": false
    },
    "number": 3,
    "created_seq": 3,
    "updated_seq": 3
  },
  {
    "id": "00000000000000000000000004",
    "author": {
      "name": "alice",
      "kind": "human"
    },
    "target": {
      "target": "lines",
      "file": "f.txt",
      "side": "after",
      "start_line": 12,
      "end_line": 12
    },
    "version": 0,
    "anchor": {
      "snippet": [
        "twelve"
      ],
      "context_before": [
        "ten",
        "ELEVEN"
      ],
      "context_after": [
        "thirteen"
      ],
      "landmark": {
        "definition": "twelve",
        "relation": "on_definition",
        "offset": 0
      }
    },
    "body": "resolved thread on twelve",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "carol",
      "kind": "human"
    },
    "resolved": true,
    "resolved_by": {
      "name": "carol",
      "kind": "human"
    },
    "resolved_at": "2024-03-01T12:00:00Z",
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c4"
    },
    "synced": {
      "body_marker": "51fb362af78f2a2904e7ff682a0bfd35216597fe290c6c9fa48f44bc4c4b4984",
      "resolved": true
    },
    "number": 4,
    "created_seq": 4,
    "updated_seq": 5
  },
  {
    "id": "00000000000000000000000005",
    "author": {
      "name": "dave",
      "kind": "human"
    },
    "target": {
      "target": "file",
      "file": "f.txt"
    },
    "version": 0,
    "anchor": null,
    "body": "line no longer in the diff",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-06-01T09:00:00Z",
    "updated_by": {
      "name": "unknown",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": true,
    "deleted_by": {
      "name": "unknown",
      "kind": "human"
    },
    "deleted_at": "2024-06-01T09:00:00Z",
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c5"
    },
    "synced": {
      "body_marker": "2c27a12863176efe1eb7e80b7f76542666e9470ed02f071c00ec155a77dcc5d6",
      "resolved": false
    },
    "number": 5,
    "created_seq": 6,
    "updated_seq": 9
  }
]"#,
    );
}

#[test]
fn a_repeated_withdrawal_appends_nothing() {
    let first = reconcile_comments(
        &fetched(),
        &[],
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let existing = fold_events(first.clone());

    // Withdraw the outdated comment upstream, then fold the withdrawal in.
    let mut later = fetched();
    later.pop();
    let second = reconcile_comments(
        &later,
        &existing,
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let mut all = first;
    all.extend(second);
    let settled = fold_events(all);

    // A further pull with the comment still absent must not tombstone it again.
    let again = reconcile_comments(
        &later,
        &settled,
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

#[test]
fn a_duplicate_origin_is_reconciled_once() {
    // A forge listing that repeats one object (an inline id colliding with an
    // issue id, or the same object across pages) must import it a single time.
    let dup = comment("c1", "alice", "said once");
    let events = reconcile_comments(
        &[dup.clone(), dup],
        &[],
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let comments = fold_events(events);
    #[rustfmt::skip]
    wince::snapshot_str!(
        as_json(&comments),
        r#"[
  {
    "id": "00000000000000000000000001",
    "author": {
      "name": "alice",
      "kind": "human"
    },
    "target": {
      "target": "review"
    },
    "version": 0,
    "anchor": null,
    "body": "said once",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "alice",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c1"
    },
    "synced": {
      "body_marker": "1f20d14ec1b5b23da694632053a2f457ef554d1a1a18d6fd54dff67489939575",
      "resolved": false
    },
    "number": 1,
    "created_seq": 1,
    "updated_seq": 1
  }
]"#,
    );
}

#[test]
fn a_reply_listed_before_its_parent_still_threads() {
    // The forge may list a reply ahead of the comment it answers; the reply
    // must still thread onto its parent rather than fall back to review level.
    let reply = FetchedComment {
        reply_to: Some(origin("c1")),
        ..comment("c2", "bob", "answering first")
    };
    let parent = comment("c1", "alice", "asked first");
    let events = reconcile_comments(
        &[reply, parent],
        &[],
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    let comments = fold_events(events);
    #[rustfmt::skip]
    wince::snapshot_str!(
        as_json(&comments),
        r#"[
  {
    "id": "00000000000000000000000001",
    "author": {
      "name": "alice",
      "kind": "human"
    },
    "target": {
      "target": "review"
    },
    "version": 0,
    "anchor": null,
    "body": "asked first",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "alice",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c1"
    },
    "synced": {
      "body_marker": "4a0143360099394e49911f524fe30f8b2d53ba464c3ed3710afde8bdac6c8c2e",
      "resolved": false
    },
    "number": 1,
    "created_seq": 1,
    "updated_seq": 1
  },
  {
    "id": "00000000000000000000000002",
    "author": {
      "name": "bob",
      "kind": "human"
    },
    "target": {
      "target": "comment",
      "id": "00000000000000000000000001"
    },
    "version": 0,
    "anchor": null,
    "body": "answering first",
    "created_at": "2024-03-01T12:00:00Z",
    "updated_at": "2024-03-01T12:00:00Z",
    "updated_by": {
      "name": "bob",
      "kind": "human"
    },
    "resolved": false,
    "resolved_by": null,
    "deleted": false,
    "deleted_by": null,
    "confidence": null,
    "origin": {
      "forge": {
        "provider": "github",
        "host": "github.com"
      },
      "kind": "review_comment",
      "id": "c2"
    },
    "synced": {
      "body_marker": "91f31fc63cae4e6b3abbc13d903403f55fccce337f7d14353e693e9b644b29df",
      "resolved": false
    },
    "number": 2,
    "created_seq": 2,
    "updated_seq": 2
  }
]"#,
    );
}

#[test]
fn a_fresh_pull_imports_each_review_verdict() {
    // An approval, a request for changes, and a plain comment review with no
    // verdict, each imported as a review-level comment; the fold derives a
    // verdict for the two that have a disposition.
    let reviews = vec![
        review(
            "r1",
            "alice",
            "looks good",
            Some(Disposition::Approve),
            false,
        ),
        review(
            "r2",
            "bob",
            "please fix",
            Some(Disposition::RequestChanges),
            false,
        ),
        review("r3", "carol", "just a note", None, false),
    ];
    let events = reconcile_reviews(&reviews, &[], VersionNumber(0), ids());
    #[rustfmt::skip]
    wince::snapshot_str!(
        fold_reviews(events),
        r#"{
  "comments": [
    {
      "anchor": null,
      "author": {
        "kind": "human",
        "name": "alice"
      },
      "body": "looks good",
      "confidence": null,
      "created_at": "2024-03-01T12:00:00Z",
      "created_seq": 1,
      "deleted": false,
      "deleted_by": null,
      "disposition": "approve",
      "id": "00000000000000000000000001",
      "number": 1,
      "origin": {
        "forge": {
          "host": "github.com",
          "provider": "github"
        },
        "id": "r1",
        "kind": "verdict"
      },
      "resolved": false,
      "resolved_by": null,
      "synced": {
        "body_marker": "858aa9275d62f2d05c2b57be7f02ad28b385f6c799be4d889b73f43deef3ea6e",
        "resolved": false
      },
      "target": {
        "target": "review"
      },
      "updated_at": "2024-03-01T12:00:00Z",
      "updated_by": {
        "kind": "human",
        "name": "alice"
      },
      "updated_seq": 1,
      "version": 0
    },
    {
      "anchor": null,
      "author": {
        "kind": "human",
        "name": "bob"
      },
      "body": "please fix",
      "confidence": null,
      "created_at": "2024-03-01T12:00:00Z",
      "created_seq": 2,
      "deleted": false,
      "deleted_by": null,
      "disposition": "request_changes",
      "id": "00000000000000000000000002",
      "number": 2,
      "origin": {
        "forge": {
          "host": "github.com",
          "provider": "github"
        },
        "id": "r2",
        "kind": "verdict"
      },
      "resolved": false,
      "resolved_by": null,
      "synced": {
        "body_marker": "363a624f921a61249eda7c59c6b08489803fcbdb15ff6fdd8b496078acf66be7",
        "resolved": false
      },
      "target": {
        "target": "review"
      },
      "updated_at": "2024-03-01T12:00:00Z",
      "updated_by": {
        "kind": "human",
        "name": "bob"
      },
      "updated_seq": 2,
      "version": 0
    },
    {
      "anchor": null,
      "author": {
        "kind": "human",
        "name": "carol"
      },
      "body": "just a note",
      "confidence": null,
      "created_at": "2024-03-01T12:00:00Z",
      "created_seq": 3,
      "deleted": false,
      "deleted_by": null,
      "id": "00000000000000000000000003",
      "number": 3,
      "origin": {
        "forge": {
          "host": "github.com",
          "provider": "github"
        },
        "id": "r3",
        "kind": "verdict"
      },
      "resolved": false,
      "resolved_by": null,
      "synced": {
        "body_marker": "c341290416ce3092349ad422614e6bd50cef1c930e349d63af6a1354a4701c73",
        "resolved": false
      },
      "target": {
        "target": "review"
      },
      "updated_at": "2024-03-01T12:00:00Z",
      "updated_by": {
        "kind": "human",
        "name": "carol"
      },
      "updated_seq": 3,
      "version": 0
    }
  ],
  "verdicts": [
    {
      "author": {
        "kind": "human",
        "name": "alice"
      },
      "disposition": "approve"
    },
    {
      "author": {
        "kind": "human",
        "name": "bob"
      },
      "disposition": "request_changes"
    }
  ]
}"#,
    );
}

#[test]
fn a_dismissed_review_imports_without_a_verdict() {
    // A review dismissed upstream keeps its summary but contributes no verdict.
    let reviews = vec![review(
        "r1",
        "alice",
        "changes requested then dismissed",
        Some(Disposition::RequestChanges),
        true,
    )];
    let events = reconcile_reviews(&reviews, &[], VersionNumber(0), ids());
    #[rustfmt::skip]
    wince::snapshot_str!(
        fold_reviews(events),
        r#"{
  "comments": [
    {
      "anchor": null,
      "author": {
        "kind": "human",
        "name": "alice"
      },
      "body": "changes requested then dismissed",
      "confidence": null,
      "created_at": "2024-03-01T12:00:00Z",
      "created_seq": 1,
      "deleted": false,
      "deleted_by": null,
      "id": "00000000000000000000000001",
      "number": 1,
      "origin": {
        "forge": {
          "host": "github.com",
          "provider": "github"
        },
        "id": "r1",
        "kind": "verdict"
      },
      "resolved": false,
      "resolved_by": null,
      "synced": {
        "body_marker": "9768b57272ce4a068cf672fc6f3ab5c13bddb943e1ee6c6e65393eece00ac722",
        "resolved": false
      },
      "target": {
        "target": "review"
      },
      "updated_at": "2024-03-01T12:00:00Z",
      "updated_by": {
        "kind": "human",
        "name": "alice"
      },
      "updated_seq": 1,
      "version": 0
    }
  ],
  "verdicts": []
}"#,
    );
}

#[test]
fn a_later_pull_tracks_a_review_edit_and_dismissal() {
    let first = reconcile_reviews(
        &[review(
            "r1",
            "alice",
            "please fix",
            Some(Disposition::RequestChanges),
            false,
        )],
        &[],
        VersionNumber(0),
        ids(),
    );
    let existing = fold_events(first.clone());

    // The summary was revised and the review was dismissed upstream, so its
    // verdict is withdrawn while the body edit is kept.
    let later = vec![review(
        "r1",
        "alice",
        "please fix (edited)",
        Some(Disposition::RequestChanges),
        true,
    )];
    let second = reconcile_reviews(&later, &existing, VersionNumber(0), ids());
    let mut all = first;
    all.extend(second);
    #[rustfmt::skip]
    wince::snapshot_str!(
        fold_reviews(all),
        r#"{
  "comments": [
    {
      "anchor": null,
      "author": {
        "kind": "human",
        "name": "alice"
      },
      "body": "please fix (edited)",
      "confidence": null,
      "created_at": "2024-03-01T12:00:00Z",
      "created_seq": 1,
      "deleted": false,
      "deleted_by": null,
      "id": "00000000000000000000000001",
      "number": 1,
      "origin": {
        "forge": {
          "host": "github.com",
          "provider": "github"
        },
        "id": "r1",
        "kind": "verdict"
      },
      "resolved": false,
      "resolved_by": null,
      "synced": {
        "body_marker": "08a1bbf5819a253262b6ed8f4e0515c31539bc5413a034a9fa3c8c5aab6359e8",
        "resolved": false
      },
      "target": {
        "target": "review"
      },
      "updated_at": "2024-03-01T12:00:00Z",
      "updated_by": {
        "kind": "human",
        "name": "alice"
      },
      "updated_seq": 3,
      "version": 0
    }
  ],
  "verdicts": []
}"#,
    );
}

#[test]
fn re_pulling_unchanged_reviews_appends_nothing() {
    let reviews = vec![review(
        "r1",
        "alice",
        "ok",
        Some(Disposition::Approve),
        false,
    )];
    let first = reconcile_reviews(&reviews, &[], VersionNumber(0), ids());
    let existing = fold_events(first);
    let again = reconcile_reviews(&reviews, &existing, VersionNumber(0), ids());
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

#[test]
fn a_content_free_review_is_not_imported() {
    // A plain comment review with no summary and no verdict, and an empty review
    // that was dismissed, are both bare containers the forge created to hold
    // inline comments; neither imports a review-level comment of its own.
    let reviews = vec![
        review("r1", "alice", "", None, false),
        review("r2", "bob", "", None, true),
    ];
    let events = reconcile_reviews(&reviews, &[], VersionNumber(0), ids());
    wince::assert_eq!(events, Vec::<RecordBody>::new());
}

#[test]
fn re_pulling_an_unchanged_review_keeps_a_local_summary_edit() {
    // A review imported with a summary, then edited locally without pushing.
    // Re-pulling the same unchanged review must not clobber the unpushed edit
    // back to the forge body: the reconcile compares the forge against the
    // synced marker, not the local body, so it appends nothing.
    let reviews = vec![review(
        "r1",
        "alice",
        "please fix",
        Some(Disposition::RequestChanges),
        false,
    )];
    let mut log = reconcile_reviews(&reviews, &[], VersionNumber(0), ids());
    let id = fold_events(log.clone())[0].id;
    log.push(edit_event(
        id,
        human("alice"),
        "please fix, and here is how".to_string(),
    ));
    let existing = fold_events(log);
    let again = reconcile_reviews(&reviews, &existing, VersionNumber(0), ids());
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

#[test]
fn a_review_listed_twice_is_reconciled_once() {
    // A forge listing that repeats one review by origin imports a single
    // verdict-level comment rather than one per repeat.
    let approve = review("r1", "alice", "ok", Some(Disposition::Approve), false);
    let reviews = vec![approve.clone(), approve];
    let events = reconcile_reviews(&reviews, &[], VersionNumber(0), ids());
    #[rustfmt::skip]
    wince::snapshot_str!(
        fold_reviews(events),
        r#"{
  "comments": [
    {
      "anchor": null,
      "author": {
        "kind": "human",
        "name": "alice"
      },
      "body": "ok",
      "confidence": null,
      "created_at": "2024-03-01T12:00:00Z",
      "created_seq": 1,
      "deleted": false,
      "deleted_by": null,
      "disposition": "approve",
      "id": "00000000000000000000000001",
      "number": 1,
      "origin": {
        "forge": {
          "host": "github.com",
          "provider": "github"
        },
        "id": "r1",
        "kind": "verdict"
      },
      "resolved": false,
      "resolved_by": null,
      "synced": {
        "body_marker": "d3cbd74709bf071c75cd0fd3a51e33486798101704e83ce228c7d53d11355fd1",
        "resolved": false
      },
      "target": {
        "target": "review"
      },
      "updated_at": "2024-03-01T12:00:00Z",
      "updated_by": {
        "kind": "human",
        "name": "alice"
      },
      "updated_seq": 1,
      "version": 0
    }
  ],
  "verdicts": [
    {
      "author": {
        "kind": "human",
        "name": "alice"
      },
      "disposition": "approve"
    }
  ]
}"#,
    );
}

#[test]
fn reconciling_comments_leaves_an_imported_verdict_untouched() {
    // A verdict-origin comment must not be withdrawn by a comment reconcile that
    // finds no matching review comment: the two kinds reconcile independently.
    let verdict = reconcile_reviews(
        &[review(
            "r1",
            "alice",
            "ok",
            Some(Disposition::Approve),
            false,
        )],
        &[],
        VersionNumber(0),
        ids(),
    );
    let existing = fold_events(verdict);
    let again = reconcile_comments(
        &[],
        &existing,
        &diff(),
        VersionNumber(0),
        &wiff_diff::SectionMatchers::builtins(),
        ids(),
    );
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

/// A description-object reference on the fake GitHub instance, the pull request
/// body's own forge object.
fn description_origin() -> ExternalRef {
    ExternalRef {
        kind: ExternalKind::Description,
        ..origin("pr-7")
    }
}

/// A forge description with `title` and `body`, bound to the pull request
/// itself as the object it mirrors.
fn fetched_description(title: &str, body: &str) -> FetchedDescription {
    FetchedDescription {
        origin: description_origin(),
        author: human("alice"),
        content: Description {
            title: title.to_string(),
            body: body.to_string(),
        },
        authored_at: datetime!(2024-03-01 12:00 UTC),
    }
}

/// A locally-authored description revision that leaves the forge binding and
/// synced marker untouched, as a human edit through the TUI makes.
fn local_edit(author: &str, title: &str, body: &str) -> RecordBody {
    RecordBody::Description(DescriptionRecord {
        author: human(author),
        authored_at: None,
        origin: None,
        synced_marker: None,
        description: Description {
            title: title.to_string(),
            body: body.to_string(),
        },
    })
}

/// Fold `events` into a review and return its current description.
fn fold_description(events: Vec<RecordBody>) -> Option<DescriptionState> {
    let mut records = vec![Record {
        seq: Seq(0),
        at: OffsetDateTime::UNIX_EPOCH,
        body: RecordBody::Session(header()),
    }];
    for (offset, body) in events.into_iter().enumerate() {
        records.push(Record {
            seq: Seq(offset as u64 + 1),
            at: datetime!(2024-06-01 09:00 UTC),
            body,
        });
    }
    fold(&records).expect("fold description events").description
}

#[test]
fn a_fresh_pull_imports_the_description() {
    let forge = fetched_description("Add the thing", "A short body.");
    let event = reconcile_description(&forge, None).expect("first contact imports upstream");
    let state = fold_description(vec![event]).expect("a description is set");
    #[rustfmt::skip]
    wince::snapshot_str!(
        serde_json::to_string_pretty(&state).expect("serialize description"),
        r#"{
  "title": "Add the thing",
  "body": "A short body.",
  "author": {
    "name": "alice",
    "kind": "human"
  },
  "updated_at": "2024-03-01T12:00:00Z",
  "origin": {
    "forge": {
      "provider": "github",
      "host": "github.com"
    },
    "kind": "description",
    "id": "pr-7"
  },
  "synced_marker": "8e5835066037bbaa39e356617eb2011fa509df8b03d074f4b5a80a25db8abcca"
}"#,
    );
}

#[test]
fn re_pulling_an_unchanged_description_appends_nothing() {
    let forge = fetched_description("Add the thing", "A short body.");
    let event = reconcile_description(&forge, None).expect("first contact imports upstream");
    let state = fold_description(vec![event]);
    let again = reconcile_description(&forge, state.as_ref());
    wince::assert_eq!(again, None);
}

#[test]
fn an_upstream_edit_reimports_the_description() {
    let first = fetched_description("Add the thing", "A short body.");
    let mut events = vec![reconcile_description(&first, None).expect("first contact imports")];
    let state = fold_description(events.clone());
    let edited = fetched_description("Add the thing", "A longer body now.");
    events
        .push(reconcile_description(&edited, state.as_ref()).expect("an upstream edit reimports"));
    let state = fold_description(events).expect("a description is set");
    #[rustfmt::skip]
    wince::snapshot_str!(
        serde_json::to_string_pretty(&state).expect("serialize description"),
        r#"{
  "title": "Add the thing",
  "body": "A longer body now.",
  "author": {
    "name": "alice",
    "kind": "human"
  },
  "updated_at": "2024-03-01T12:00:00Z",
  "origin": {
    "forge": {
      "provider": "github",
      "host": "github.com"
    },
    "kind": "description",
    "id": "pr-7"
  },
  "synced_marker": "33e541791308c15fc88ecf55bfe42a95f7c5f39e5c3c80cbefb1b8a21ee5c04e"
}"#,
    );
}

#[test]
fn a_local_edit_against_an_untouched_upstream_is_preserved() {
    // Import upstream, edit locally, then re-pull with upstream unchanged: the
    // synced marker still names the imported content, so nothing reimports and
    // the local edit stands for the next push.
    let upstream = fetched_description("Add the thing", "Upstream body.");
    let mut events = vec![reconcile_description(&upstream, None).expect("first contact imports")];
    events.push(local_edit("alice", "Add the thing", "Local body."));
    let state = fold_description(events);
    let again = reconcile_description(&upstream, state.as_ref());
    wince::assert_eq!(again, None);
}
