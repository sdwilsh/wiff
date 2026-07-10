#![allow(missing_docs)]

use std::path::Path;

use ulid::Ulid;
use wiff_core::record::{Anchor, Author, AuthorKind, CommentTarget, SourceKind};
use wiff_core::review::{CommentState, fold};
use wiff_core::session::read_records;
use wiff_core::{
    CapturedDiff, DraftComment, ProjectIdentity, RefreshOutcome, SessionLog, create_session,
    refresh_session,
};
use wiff_diff::{LineNo, Side};

/// v0: a four-line added file. The whole after side is present, so line numbers
/// reconstruct cleanly.
const V0: &str = "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+alpha
+beta
+gamma
+delta
";

fn identity() -> ProjectIdentity {
    ProjectIdentity {
        canonical: "demo".to_string(),
        repo_root: None,
        scm: None,
    }
}

/// Create a session whose v0 is [`V0`] and attach a comment to line 3 (`gamma`)
/// on the after side, returning the session and the comment's id.
fn session_with_gamma_comment() -> (tempfile::TempDir, SessionLog, Ulid) {
    let base = tempfile::tempdir().unwrap();
    let captured = CapturedDiff {
        text: V0.to_string(),
        source: SourceKind::Stdin,
    };
    let mut log = create_session(base.path(), &identity(), Path::new("/work"), &captured).unwrap();
    let added = DraftComment {
        author: Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        },
        target: CommentTarget::Lines {
            file: "f.txt".to_string(),
            side: Side::After,
            start_line: LineNo::new(3).unwrap(),
            end_line: LineNo::new(3).unwrap(),
        },
        body: "why gamma?".to_string(),
    }
    .append(&mut log)
    .unwrap();
    (base, log, added.id)
}

fn lines_target(start: u32, end: u32) -> CommentTarget {
    CommentTarget::Lines {
        file: "f.txt".to_string(),
        side: Side::After,
        start_line: LineNo::new(start).unwrap(),
        end_line: LineNo::new(end).unwrap(),
    }
}

/// The single comment's folded state after a refresh.
fn comment_after_refresh(log: &SessionLog) -> CommentState {
    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    let mut comments = state.comments;
    k9::assert_equal!(comments.len(), 1);
    comments.pop().unwrap()
}

fn expected_comment(
    id: Ulid,
    target: CommentTarget,
    version: u32,
    confidence: Option<wiff_core::record::Confidence>,
    updated_seq: u64,
) -> CommentState {
    CommentState {
        id,
        author: Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        },
        target,
        version,
        anchor: Some(Anchor {
            snippet: vec!["gamma".to_string()],
            context_before: vec!["alpha".to_string(), "beta".to_string()],
            context_after: vec!["delta".to_string()],
        }),
        body: "why gamma?".to_string(),
        resolved: false,
        resolved_by: None,
        deleted: false,
        deleted_by: None,
        confidence,
        created_seq: 2,
        updated_seq,
    }
}

#[test]
fn an_unchanged_line_shifts_to_its_new_position_exactly() {
    let (_base, mut log, id) = session_with_gamma_comment();
    // A line is inserted at the top, so gamma slides from line 3 to line 4.
    let v1 = "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,5 @@
+zero
+alpha
+beta
+gamma
+delta
";
    let outcome = refresh_session(&mut log, v1).unwrap();
    k9::assert_equal!(
        outcome,
        Some(RefreshOutcome {
            version: 1,
            exact: 1,
            approximate: 0,
            outdated: 0,
        })
    );
    k9::assert_equal!(
        comment_after_refresh(&log),
        expected_comment(
            id,
            lines_target(4, 4),
            1,
            Some(wiff_core::record::Confidence::Exact),
            4,
        )
    );
}

#[test]
fn a_changed_line_becomes_outdated_pinned_in_place() {
    let (_base, mut log, id) = session_with_gamma_comment();
    // gamma's own text changes, so it cannot be located.
    let v1 = "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+alpha
+beta
+gamma is now different
+delta
";
    let outcome = refresh_session(&mut log, v1).unwrap();
    k9::assert_equal!(
        outcome,
        Some(RefreshOutcome {
            version: 1,
            exact: 0,
            approximate: 0,
            outdated: 1,
        })
    );
    k9::assert_equal!(
        comment_after_refresh(&log),
        expected_comment(
            id,
            lines_target(3, 3),
            1,
            Some(wiff_core::record::Confidence::Outdated),
            4,
        )
    );
}

#[test]
fn a_relocated_line_is_found_approximately() {
    let (_base, mut log, id) = session_with_gamma_comment();
    // gamma moves to the top; its old slot is taken by other equal lines, so
    // offset mapping fails but the snippet is found relocated.
    let v1 = "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+gamma
+alpha
+beta
+delta
";
    let outcome = refresh_session(&mut log, v1).unwrap();
    k9::assert_equal!(
        outcome,
        Some(RefreshOutcome {
            version: 1,
            exact: 0,
            approximate: 1,
            outdated: 0,
        })
    );
    k9::assert_equal!(
        comment_after_refresh(&log),
        expected_comment(
            id,
            lines_target(1, 1),
            1,
            Some(wiff_core::record::Confidence::Approximate),
            4,
        )
    );
}

#[test]
fn an_identical_diff_captures_nothing() {
    let (_base, mut log, id) = session_with_gamma_comment();
    let outcome = refresh_session(&mut log, V0).unwrap();
    k9::assert_equal!(outcome, None);
    // The comment stays anchored to v0, untouched.
    k9::assert_equal!(
        comment_after_refresh(&log),
        expected_comment(id, lines_target(3, 3), 0, None, 2)
    );
}
