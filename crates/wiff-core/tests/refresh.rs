#![allow(missing_docs)]

use std::path::Path;

use time::OffsetDateTime;
use ulid::Ulid;
use wiff_core::record::{
    Anchor, Author, AuthorKind, CommentTarget, Seq, SourceKind, VersionNumber,
};
use wiff_core::review::{CommentState, fold};
use wiff_core::session::read_records;
use wiff_core::{
    CapturedDiff, DraftComment, LockWait, ProjectIdentity, RefreshOutcome, SessionLog,
    create_session, refresh_session,
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
    let mut log = create_session(
        base.path(),
        &identity(),
        Path::new("/work"),
        &captured,
        None,
    )
    .unwrap();
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
    .append(&mut log, LockWait::Block)
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

/// The human reviewer who authors the comment and, in most tests, also runs the
/// refresh, so a reanchor's `updated_by` matches the original author.
fn author() -> Author {
    Author {
        name: "wez".to_string(),
        kind: AuthorKind::Human,
    }
}

/// An agent, distinct from the comment's human author, that runs a refresh in
/// the differing-actor test.
fn agent() -> Author {
    Author {
        name: "opus".to_string(),
        kind: AuthorKind::Agent,
    }
}

/// The single comment's folded state after a refresh, with its wall-clock
/// timestamps normalized to the epoch so the state asserts deterministically.
fn comment_after_refresh(log: &SessionLog) -> CommentState {
    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    let mut comments = state.comments;
    wince::assert_eq!(comments.len(), 1);
    let mut comment = comments.pop().unwrap();
    comment.created_at = OffsetDateTime::UNIX_EPOCH;
    comment.updated_at = OffsetDateTime::UNIX_EPOCH;
    comment
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
        author: author(),
        target,
        version: VersionNumber(version),
        anchor: Some(Anchor {
            snippet: vec!["gamma".to_string()],
            context_before: vec!["alpha".to_string(), "beta".to_string()],
            context_after: vec!["delta".to_string()],
        }),
        body: "why gamma?".to_string(),
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
        updated_by: author(),
        resolved: false,
        resolved_by: None,
        resolved_at: None,
        deleted: false,
        deleted_by: None,
        deleted_at: None,
        confidence,
        origin: None,
        synced_marker: None,
        created_seq: Seq(2),
        updated_seq: Seq(updated_seq),
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
    let outcome = refresh_session(&mut log, v1, author(), LockWait::Block).unwrap();
    wince::assert_eq!(
        outcome,
        Some(RefreshOutcome {
            version: VersionNumber(1),
            exact: 1,
            approximate: 0,
            outdated: 0,
        })
    );
    wince::assert_eq!(
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
    let outcome = refresh_session(&mut log, v1, author(), LockWait::Block).unwrap();
    wince::assert_eq!(
        outcome,
        Some(RefreshOutcome {
            version: VersionNumber(1),
            exact: 0,
            approximate: 0,
            outdated: 1,
        })
    );
    wince::assert_eq!(
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
    let outcome = refresh_session(&mut log, v1, author(), LockWait::Block).unwrap();
    wince::assert_eq!(
        outcome,
        Some(RefreshOutcome {
            version: VersionNumber(1),
            exact: 0,
            approximate: 1,
            outdated: 0,
        })
    );
    wince::assert_eq!(
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
fn a_reanchor_by_a_different_actor_does_not_attribute_a_change() {
    let (_base, mut log, id) = session_with_gamma_comment();
    // A line is inserted at the top, so gamma slides from line 3 to line 4. An
    // agent runs the refresh, but a reanchor is automatic bookkeeping: the
    // comment keeps its original human author and reports no last changer, even
    // though its update sequence advances so the move is still detected.
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
    let outcome = refresh_session(&mut log, v1, agent(), LockWait::Block).unwrap();
    wince::assert_eq!(
        outcome,
        Some(RefreshOutcome {
            version: VersionNumber(1),
            exact: 1,
            approximate: 0,
            outdated: 0,
        })
    );
    let comment = comment_after_refresh(&log);
    let last_changed = comment.last_changed_by().cloned();
    // The reanchor advances the update sequence but leaves the author in place
    // as the updater, so `expected_comment`'s default `updated_by` of the
    // human author is correct.
    let expected = expected_comment(
        id,
        lines_target(4, 4),
        1,
        Some(wiff_core::record::Confidence::Exact),
        4,
    );
    wince::assert_eq!(comment, expected);
    // A reanchor by a different actor is not reported as a last change.
    wince::assert_eq!(last_changed, None);
}

#[test]
fn a_refresh_reanchors_the_parent_but_leaves_its_reply_in_place() {
    // A reply has no anchor of its own, so a refresh that moves its parent's
    // line reanchors the parent and never emits a reanchor for the reply: the
    // reply keeps its parent target, its authored-against version, and the
    // update sequence it was created at.
    let (_base, mut log, parent) = session_with_gamma_comment();
    let reply = DraftComment {
        author: agent(),
        target: CommentTarget::Comment { id: parent },
        body: "seconded".to_string(),
    }
    .append(&mut log, LockWait::Block)
    .unwrap()
    .id;
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
    let outcome = refresh_session(&mut log, v1, author(), LockWait::Block).unwrap();
    // Only the parent line comment rebases; the reply is not counted.
    wince::assert_eq!(
        outcome,
        Some(RefreshOutcome {
            version: VersionNumber(1),
            exact: 1,
            approximate: 0,
            outdated: 0,
        })
    );
    let state = fold(&read_records(log.path()).unwrap()).unwrap();
    let placed: Vec<(Ulid, CommentTarget, Option<Ulid>, u32, u64, u64)> = state
        .comments
        .iter()
        .map(|comment| {
            (
                comment.id,
                comment.target.clone(),
                comment.reply_to(),
                comment.version.get(),
                comment.created_seq.get(),
                comment.updated_seq.get(),
            )
        })
        .collect();
    wince::assert_eq!(
        placed,
        vec![
            (parent, lines_target(4, 4), None, 1, 2, 5),
            (
                reply,
                CommentTarget::Comment { id: parent },
                Some(parent),
                0,
                3,
                3
            ),
        ]
    );
}

#[test]
fn an_identical_diff_captures_nothing() {
    let (_base, mut log, id) = session_with_gamma_comment();
    let outcome = refresh_session(&mut log, V0, author(), LockWait::Block).unwrap();
    wince::assert_eq!(outcome, None);
    // The comment stays anchored to v0, untouched.
    wince::assert_eq!(
        comment_after_refresh(&log),
        expected_comment(id, lines_target(3, 3), 0, None, 2)
    );
}
