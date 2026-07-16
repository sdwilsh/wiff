#![allow(missing_docs)]

use std::path::Path;

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_core::record::{
    Anchor, Author, AuthorKind, CommentTarget, DiffVersionRecord, FORMAT_VERSION, FileSummary, Seq,
    SessionHeader, SourceKind, VersionNumber,
};
use wiff_core::review::{CommentState, fold};
use wiff_core::session::read_records;
use wiff_core::{
    CapturedDiff, DraftComment, Error, LockWait, ProjectIdentity, SessionLog, SidebandHash,
    create_session, delete_comment, set_resolved,
};
use wiff_diff::{FileStatus, LineNo, Side};

const DIFF: &str = "\
diff --git a/added.txt b/added.txt
new file mode 100644
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,2 @@
+first
+second
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;
";

fn identity() -> ProjectIdentity {
    ProjectIdentity {
        canonical: "demo".to_string(),
        repo_root: None,
        scm: None,
    }
}

fn session() -> (tempfile::TempDir, SessionLog) {
    let base = tempfile::tempdir().unwrap();
    let captured = CapturedDiff {
        text: DIFF.to_string(),
        source: SourceKind::Stdin,
    };
    let log = create_session(
        base.path(),
        &identity(),
        Path::new("/work"),
        &captured,
        None,
    )
    .unwrap();
    (base, log)
}

fn human(name: &str) -> Author {
    Author {
        name: name.to_string(),
        kind: AuthorKind::Human,
    }
}

fn lines_target(file: &str, start: u32, end: u32) -> CommentTarget {
    CommentTarget::Lines {
        file: file.to_string(),
        side: Side::After,
        start_line: LineNo::new(start).unwrap(),
        end_line: LineNo::new(end).unwrap(),
    }
}

/// Normalize each comment's wall-clock timestamps to the epoch so a folded
/// state asserts deterministically; a present resolve/delete time is kept
/// present, only flattened.
fn normalize(mut comments: Vec<CommentState>) -> Vec<CommentState> {
    for comment in &mut comments {
        comment.created_at = OffsetDateTime::UNIX_EPOCH;
        comment.updated_at = OffsetDateTime::UNIX_EPOCH;
        if comment.resolved_at.is_some() {
            comment.resolved_at = Some(OffsetDateTime::UNIX_EPOCH);
        }
        if comment.deleted_at.is_some() {
            comment.deleted_at = Some(OffsetDateTime::UNIX_EPOCH);
        }
    }
    comments
}

#[test]
fn adding_comments_captures_anchors_and_folds_to_current_state() {
    let (_base, mut log) = session();

    let line = DraftComment {
        author: human("wez"),
        target: lines_target("src/main.rs", 2, 2),
        body: "why 3?".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();
    let whole = DraftComment {
        author: Author {
            name: "assistant".to_string(),
            kind: AuthorKind::Agent,
        },
        target: CommentTarget::File {
            file: "added.txt".to_string(),
        },
        body: "needs a newline".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();
    let overall = DraftComment {
        author: human("wez"),
        target: CommentTarget::Review,
        body: "looks good".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();

    // The line comment captured the changed line and one context line each side.
    wince::assert_eq!(
        line.anchor,
        Some(Anchor {
            snippet: vec!["let b = 3;".to_string()],
            context_before: vec!["let a = 1;".to_string()],
            context_after: vec!["let c = 4;".to_string()],
        })
    );
    wince::assert_eq!(whole.anchor, None);
    wince::assert_eq!(overall.anchor, None);
    wince::assert_eq!((line.seq, line.version), (Seq(2), VersionNumber(0)));
    wince::assert_eq!((whole.seq, whole.version), (Seq(3), VersionNumber(0)));
    wince::assert_eq!((overall.seq, overall.version), (Seq(4), VersionNumber(0)));

    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    let expected_header = SessionHeader {
        ulid: log.ulid(),
        version: FORMAT_VERSION,
        project: "demo".to_string(),
        repo_root: None,
        cwd: "/work".to_string(),
        source: SourceKind::Stdin,
    };
    wince::assert_eq!(state.session, expected_header);
    wince::assert_eq!(
        state.versions,
        vec![DiffVersionRecord {
            number: VersionNumber(0),
            diff_hash: SidebandHash::of(DIFF.as_bytes()),
            files: vec![
                FileSummary {
                    old_path: "added.txt".to_string(),
                    new_path: "added.txt".to_string(),
                    status: FileStatus::Added,
                    hunk_count: 1,
                },
                FileSummary {
                    old_path: "src/main.rs".to_string(),
                    new_path: "src/main.rs".to_string(),
                    status: FileStatus::Modified,
                    hunk_count: 1,
                },
            ],
        }]
    );
    wince::assert_eq!(
        normalize(state.comments),
        vec![
            CommentState {
                id: line.id,
                author: human("wez"),
                target: lines_target("src/main.rs", 2, 2),
                version: VersionNumber(0),
                anchor: Some(Anchor {
                    snippet: vec!["let b = 3;".to_string()],
                    context_before: vec!["let a = 1;".to_string()],
                    context_after: vec!["let c = 4;".to_string()],
                }),
                body: "why 3?".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                updated_by: human("wez"),
                resolved: false,
                resolved_by: None,
                resolved_at: None,
                deleted: false,
                deleted_by: None,
                deleted_at: None,
                confidence: None,
                origin: None,
                synced_marker: None,
                created_seq: Seq(2),
                updated_seq: Seq(2),
            },
            CommentState {
                id: whole.id,
                author: Author {
                    name: "assistant".to_string(),
                    kind: AuthorKind::Agent,
                },
                target: CommentTarget::File {
                    file: "added.txt".to_string(),
                },
                version: VersionNumber(0),
                anchor: None,
                body: "needs a newline".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                updated_by: Author {
                    name: "assistant".to_string(),
                    kind: AuthorKind::Agent,
                },
                resolved: false,
                resolved_by: None,
                resolved_at: None,
                deleted: false,
                deleted_by: None,
                deleted_at: None,
                confidence: None,
                origin: None,
                synced_marker: None,
                created_seq: Seq(3),
                updated_seq: Seq(3),
            },
            CommentState {
                id: overall.id,
                author: human("wez"),
                target: CommentTarget::Review,
                version: VersionNumber(0),
                anchor: None,
                body: "looks good".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                updated_by: human("wez"),
                resolved: false,
                resolved_by: None,
                resolved_at: None,
                deleted: false,
                deleted_by: None,
                deleted_at: None,
                confidence: None,
                origin: None,
                synced_marker: None,
                created_seq: Seq(4),
                updated_seq: Seq(4),
            },
        ]
    );
}

#[test]
fn a_line_range_anchors_across_multiple_lines() {
    let (_base, mut log) = session();
    let added = DraftComment {
        author: human("dev"),
        target: lines_target("added.txt", 1, 2),
        body: "both lines".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();
    wince::assert_eq!(
        added.anchor,
        Some(Anchor {
            snippet: vec!["first".to_string(), "second".to_string()],
            context_before: vec![],
            context_after: vec![],
        })
    );
}

#[test]
fn a_line_beyond_the_captured_window_is_recorded_without_an_anchor() {
    let (_base, mut log) = session();
    let added = DraftComment {
        author: human("wez"),
        target: lines_target("src/main.rs", 100, 100),
        body: "look here for context".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();
    wince::assert_eq!(added.anchor, None);

    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    wince::assert_eq!(
        normalize(state.comments),
        vec![CommentState {
            id: added.id,
            author: human("wez"),
            target: lines_target("src/main.rs", 100, 100),
            version: VersionNumber(0),
            anchor: None,
            body: "look here for context".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: human("wez"),
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            created_seq: Seq(2),
            updated_seq: Seq(2),
        }]
    );
}

#[test]
fn resolving_and_withdrawing_comments_folds_to_current_state() {
    let (_base, mut log) = session();
    let keep = DraftComment {
        author: human("wez"),
        target: CommentTarget::File {
            file: "added.txt".to_string(),
        },
        body: "needs a newline".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();
    let gone = DraftComment {
        author: human("wez"),
        target: CommentTarget::Review,
        body: "never mind".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();

    // Resolve then reopen the kept comment; its state reflects the last write.
    set_resolved(&mut log, keep.id, true, human("wez"), LockWait::Block).unwrap();
    set_resolved(&mut log, keep.id, false, human("wez"), LockWait::Block).unwrap();
    delete_comment(&mut log, gone.id, human("wez"), LockWait::Block).unwrap();

    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    wince::assert_eq!(
        normalize(state.comments),
        vec![
            CommentState {
                id: keep.id,
                author: human("wez"),
                target: CommentTarget::File {
                    file: "added.txt".to_string(),
                },
                version: VersionNumber(0),
                anchor: None,
                body: "needs a newline".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                updated_by: human("wez"),
                resolved: false,
                resolved_by: Some(human("wez")),
                resolved_at: Some(OffsetDateTime::UNIX_EPOCH),
                deleted: false,
                deleted_by: None,
                deleted_at: None,
                confidence: None,
                origin: None,
                synced_marker: None,
                created_seq: Seq(2),
                updated_seq: Seq(5),
            },
            CommentState {
                id: gone.id,
                author: human("wez"),
                target: CommentTarget::Review,
                version: VersionNumber(0),
                anchor: None,
                body: "never mind".to_string(),
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
                updated_by: human("wez"),
                resolved: false,
                resolved_by: None,
                resolved_at: None,
                deleted: true,
                deleted_by: Some(human("wez")),
                deleted_at: Some(OffsetDateTime::UNIX_EPOCH),
                confidence: None,
                origin: None,
                synced_marker: None,
                created_seq: Seq(3),
                updated_seq: Seq(6),
            },
        ]
    );
}

#[test]
fn mutating_an_unknown_comment_is_an_error() {
    let (_base, mut log) = session();
    let missing = Ulid::new();
    let resolve_err =
        set_resolved(&mut log, missing, true, human("wez"), LockWait::Block).unwrap_err();
    let delete_err = delete_comment(&mut log, missing, human("wez"), LockWait::Block).unwrap_err();
    wince::assert_eq!(
        resolve_err.to_string(),
        format!("no comment {missing} in this session")
    );
    wince::assert_eq!(
        delete_err.to_string(),
        format!("no comment {missing} in this session")
    );
}

#[test]
fn a_reply_names_its_parent_and_folds_beneath_it() {
    let (_base, mut log) = session();
    let root = DraftComment {
        author: human("wez"),
        target: CommentTarget::Review,
        body: "why 3?".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();
    let reply = DraftComment {
        author: human("dev"),
        target: CommentTarget::Comment { id: root.id },
        body: "it is the bound".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();
    wince::assert_eq!(reply.anchor, None);

    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    let threaded: Vec<(Ulid, Option<Ulid>, CommentTarget)> = state
        .comments
        .iter()
        .map(|comment| (comment.id, comment.reply_to(), comment.target.clone()))
        .collect();
    wince::assert_eq!(
        threaded,
        vec![
            (root.id, None, CommentTarget::Review),
            (
                reply.id,
                Some(root.id),
                CommentTarget::Comment { id: root.id }
            ),
        ]
    );
}

#[test]
fn a_reply_to_an_unknown_comment_is_rejected_before_it_corrupts_the_session() {
    // Fold treats a reply to an unknown id as a corrupt log, so appending one
    // unchecked would make the whole session unreadable. `append` must refuse
    // the reply and leave the log foldable.
    let (_base, mut log) = session();
    let missing = Ulid::new();
    let error = DraftComment {
        author: human("wez"),
        target: CommentTarget::Comment { id: missing },
        body: "reply to nothing".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap_err();
    wince::assert_eq!(matches!(error, Error::UnknownComment(_)), true);
    wince::assert_eq!(
        error.to_string(),
        format!("no comment {missing} in this session")
    );

    // The refused reply never reached the log, so it still folds cleanly.
    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    wince::assert_eq!(state.comments, Vec::<CommentState>::new());
}

#[test]
fn a_reply_to_a_withdrawn_comment_is_refused_at_authoring() {
    // Authoring a fresh reply to an already-withdrawn comment is refused locally;
    // a sync from another actor may still fold a reply under a parent withdrawn
    // after the reply was written, but a local author sees the tombstone and is
    // stopped before adding an orphaned comment.
    let (_base, mut log) = session();
    let root = DraftComment {
        author: human("wez"),
        target: CommentTarget::Review,
        body: "why 3?".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap();
    delete_comment(&mut log, root.id, human("wez"), LockWait::Block).unwrap();

    let error = DraftComment {
        author: human("dev"),
        target: CommentTarget::Comment { id: root.id },
        body: "too late".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap_err();
    wince::assert_eq!(matches!(error, Error::WithdrawnComment(_)), true);
    wince::assert_eq!(
        error.to_string(),
        format!("cannot reply to withdrawn comment {}", root.id)
    );

    // The refused reply never reached the log; only the withdrawn root remains.
    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    let ids: Vec<(Ulid, bool)> = state
        .comments
        .iter()
        .map(|comment| (comment.id, comment.deleted))
        .collect();
    wince::assert_eq!(ids, vec![(root.id, true)]);
}

#[test]
fn a_file_outside_the_diff_cannot_be_anchored() {
    let (_base, mut log) = session();
    let error = DraftComment {
        author: human("wez"),
        target: lines_target("nope.rs", 1, 1),
        body: "nowhere".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap_err();
    wince::assert_eq!(matches!(error, Error::Anchor(_)), true);
    wince::snapshot_display!(
        error,
        "cannot anchor comment: nope.rs is not part of diff v0"
    );
}
