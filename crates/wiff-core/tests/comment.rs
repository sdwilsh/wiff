#![allow(missing_docs)]

use std::path::Path;

use wiff_core::record::{
    Anchor, Author, AuthorKind, CommentTarget, DiffVersionRecord, FileSummary, SessionHeader,
    SourceKind,
};
use wiff_core::review::{CommentState, fold};
use wiff_core::session::read_records;
use wiff_core::{
    CapturedDiff, DraftComment, Error, ProjectIdentity, SessionLog, SidebandHash, create_session,
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
    let log = create_session(base.path(), &identity(), Path::new("/work"), &captured).unwrap();
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

#[test]
fn adding_comments_captures_anchors_and_folds_to_current_state() {
    let (_base, mut log) = session();

    let line = DraftComment {
        author: human("wez"),
        target: lines_target("src/main.rs", 2, 2),
        body: "why 3?".to_string(),
    }
    .append(&mut log)
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
    .append(&mut log)
    .unwrap();
    let overall = DraftComment {
        author: human("wez"),
        target: CommentTarget::Review,
        body: "looks good".to_string(),
    }
    .append(&mut log)
    .unwrap();

    // The line comment captured the changed line and one context line each side.
    k9::assert_equal!(
        line.anchor,
        Some(Anchor {
            snippet: vec!["let b = 3;".to_string()],
            context_before: vec!["let a = 1;".to_string()],
            context_after: vec!["let c = 4;".to_string()],
        })
    );
    k9::assert_equal!(whole.anchor, None);
    k9::assert_equal!(overall.anchor, None);
    k9::assert_equal!((line.seq, line.version), (2, 0));
    k9::assert_equal!((whole.seq, whole.version), (3, 0));
    k9::assert_equal!((overall.seq, overall.version), (4, 0));

    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    let expected_header = SessionHeader {
        ulid: log.ulid(),
        version: 1,
        project: "demo".to_string(),
        repo_root: None,
        cwd: "/work".to_string(),
        source: SourceKind::Stdin,
    };
    k9::assert_equal!(state.session, expected_header);
    k9::assert_equal!(
        state.versions,
        vec![DiffVersionRecord {
            number: 0,
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
    k9::assert_equal!(
        state.comments,
        vec![
            CommentState {
                id: line.id,
                author: human("wez"),
                target: lines_target("src/main.rs", 2, 2),
                version: 0,
                anchor: Some(Anchor {
                    snippet: vec!["let b = 3;".to_string()],
                    context_before: vec!["let a = 1;".to_string()],
                    context_after: vec!["let c = 4;".to_string()],
                }),
                body: "why 3?".to_string(),
                resolved: false,
                deleted: false,
                confidence: None,
                created_seq: 2,
                updated_seq: 2,
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
                version: 0,
                anchor: None,
                body: "needs a newline".to_string(),
                resolved: false,
                deleted: false,
                confidence: None,
                created_seq: 3,
                updated_seq: 3,
            },
            CommentState {
                id: overall.id,
                author: human("wez"),
                target: CommentTarget::Review,
                version: 0,
                anchor: None,
                body: "looks good".to_string(),
                resolved: false,
                deleted: false,
                confidence: None,
                created_seq: 4,
                updated_seq: 4,
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
    .append(&mut log)
    .unwrap();
    k9::assert_equal!(
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
    .append(&mut log)
    .unwrap();
    k9::assert_equal!(added.anchor, None);

    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    k9::assert_equal!(
        state.comments,
        vec![CommentState {
            id: added.id,
            author: human("wez"),
            target: lines_target("src/main.rs", 100, 100),
            version: 0,
            anchor: None,
            body: "look here for context".to_string(),
            resolved: false,
            deleted: false,
            confidence: None,
            created_seq: 2,
            updated_seq: 2,
        }]
    );
}

#[test]
fn a_file_outside_the_diff_cannot_be_anchored() {
    let (_base, mut log) = session();
    let error = DraftComment {
        author: human("wez"),
        target: lines_target("nope.rs", 1, 1),
        body: "nowhere".to_string(),
    }
    .append(&mut log)
    .unwrap_err();
    k9::assert_equal!(matches!(error, Error::Anchor(_)), true);
    k9::assert_equal!(
        error.to_string(),
        "cannot anchor comment: nope.rs is not part of diff v0".to_string()
    );
}
