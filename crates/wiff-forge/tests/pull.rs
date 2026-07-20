#![allow(missing_docs)]

use time::OffsetDateTime;
use time::macros::datetime;
use ulid::Ulid;
use wiff_core::record::{
    Author, AuthorKind, ExternalKind, ExternalRef, FORMAT_VERSION, ForgeId, Record, RecordBody,
    RevisionId, ScmSource, Seq, SessionHeader, SourceKind, TipRule, VersionNumber,
};
use wiff_core::review::{CommentState, fold};
use wiff_core::{BaseRuleset, ScmType};
use wiff_diff::parse::parse;
use wiff_diff::{Diff, LineNo, Side};
use wiff_forge::reconcile_comments;
use wiff_forge::types::{FetchedComment, ForgeAnchor, Resolution};

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
            tip: TipRule::Worktree,
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
    let events = reconcile_comments(&fetched(), &[], &diff(), VersionNumber(0), ids());
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
      ]
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
      ]
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
    "number": 5,
    "created_seq": 6,
    "updated_seq": 6
  }
]"#,
    );
}

#[test]
fn re_pulling_an_unchanged_request_appends_nothing() {
    let first = reconcile_comments(&fetched(), &[], &diff(), VersionNumber(0), ids());
    let existing = fold_events(first);
    let again = reconcile_comments(&fetched(), &existing, &diff(), VersionNumber(0), ids());
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

#[test]
fn a_later_pull_edits_resolves_and_withdraws() {
    let first = reconcile_comments(&fetched(), &[], &diff(), VersionNumber(0), ids());
    let existing = fold_events(first.clone());

    // The review-level comment's body changed, its thread is now resolved, and
    // the outdated inline comment was deleted upstream.
    let mut later = fetched();
    later[1].body = "on reflection, request changes".to_string();
    later[1].resolution = Some(Resolution {
        by: Some(human("bob")),
    });
    later.pop();

    let second = reconcile_comments(&later, &existing, &diff(), VersionNumber(0), ids());
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
      ]
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
      ]
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
    "number": 5,
    "created_seq": 6,
    "updated_seq": 9
  }
]"#,
    );
}

#[test]
fn a_repeated_withdrawal_appends_nothing() {
    let first = reconcile_comments(&fetched(), &[], &diff(), VersionNumber(0), ids());
    let existing = fold_events(first.clone());

    // Withdraw the outdated comment upstream, then fold the withdrawal in.
    let mut later = fetched();
    later.pop();
    let second = reconcile_comments(&later, &existing, &diff(), VersionNumber(0), ids());
    let mut all = first;
    all.extend(second);
    let settled = fold_events(all);

    // A further pull with the comment still absent must not tombstone it again.
    let again = reconcile_comments(&later, &settled, &diff(), VersionNumber(0), ids());
    wince::assert_eq!(again, Vec::<RecordBody>::new());
}

#[test]
fn a_duplicate_origin_is_reconciled_once() {
    // A forge listing that repeats one object (an inline id colliding with an
    // issue id, or the same object across pages) must import it a single time.
    let dup = comment("c1", "alice", "said once");
    let events = reconcile_comments(&[dup.clone(), dup], &[], &diff(), VersionNumber(0), ids());
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
    let events = reconcile_comments(&[reply, parent], &[], &diff(), VersionNumber(0), ids());
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
    "number": 2,
    "created_seq": 2,
    "updated_seq": 2
  }
]"#,
    );
}
