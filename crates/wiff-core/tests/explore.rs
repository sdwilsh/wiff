#![allow(missing_docs)]

use std::path::Path;

use ulid::Ulid;
use wiff_core::record::{Author, AuthorKind, CommentTarget, Confidence, SourceKind, VersionNumber};
use wiff_core::review::{CommentState, fold};
use wiff_core::session::read_records;
use wiff_core::{
    DraftComment, LockWait, ProjectIdentity, SessionLog, SkipReason, capture_explore,
    create_session, refresh_session, widen_explore,
};
use wiff_diff::parse::parse;
use wiff_diff::{FileStatus, LineKind, LineNo, Side};

/// Write `content` to `name` under `root`, creating parent directories.
fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, content).expect("write file");
}

#[test]
fn a_set_of_files_captures_as_an_all_context_diff() {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), "foo.txt", "alpha\nbeta\ngamma\n");
    write(root.path(), "sub/bar.txt", "one\ntwo\n");

    let capture = capture_explore(root.path(), &["foo.txt".into(), "sub/bar.txt".into()]);

    // The synthesized diff presents each file whole, every line as context, with
    // the files in sorted path order.
    wince::assert_eq!(
        capture.captured.text,
        "\
diff --git a/foo.txt b/foo.txt
--- a/foo.txt
+++ b/foo.txt
@@ -1,3 +1,3 @@
 alpha
 beta
 gamma
diff --git a/sub/bar.txt b/sub/bar.txt
--- a/sub/bar.txt
+++ b/sub/bar.txt
@@ -1,2 +1,2 @@
 one
 two
"
        .to_string()
    );
    wince::assert_eq!(capture.captured.source, SourceKind::Explore);
    wince::assert_eq!(capture.skipped, Vec::new());
}

#[test]
fn the_synthesized_diff_parses_to_all_context_files() {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), "foo.txt", "alpha\nbeta\n");

    let capture = capture_explore(root.path(), &["foo.txt".into()]);
    let diff = parse(&capture.captured.text).expect("parse");

    // One modified file, one hunk, both lines context on both sides.
    let file = &diff.files[0];
    let kinds: Vec<LineKind> = file.hunks[0].lines.iter().map(|line| line.kind).collect();
    wince::assert_eq!(
        (
            diff.files.len(),
            file.new_path.clone(),
            file.status,
            file.hunks.len(),
            kinds,
        ),
        (
            1,
            "foo.txt".to_string(),
            FileStatus::Modified,
            1,
            vec![LineKind::Context, LineKind::Context],
        )
    );
}

#[test]
fn path_order_does_not_change_the_captured_bytes() {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), "a.txt", "a\n");
    write(root.path(), "b.txt", "b\n");

    let one = capture_explore(root.path(), &["a.txt".into(), "b.txt".into()]);
    let two = capture_explore(
        root.path(),
        &["b.txt".into(), "a.txt".into(), "a.txt".into()],
    );

    // Sorting and deduplicating the paths first makes the same set yield
    // byte-identical text, so a re-add of an existing file is recognized as a
    // no-op by content hash.
    wince::assert_eq!(one.captured.text, two.captured.text);
}

#[test]
fn an_empty_file_captures_as_headers_with_no_hunk() {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), "empty.txt", "");

    let capture = capture_explore(root.path(), &["empty.txt".into()]);

    wince::assert_eq!(
        capture.captured.text,
        "\
diff --git a/empty.txt b/empty.txt
--- a/empty.txt
+++ b/empty.txt
"
        .to_string()
    );
}

#[test]
fn an_empty_set_captures_nothing() {
    let root = tempfile::tempdir().expect("tempdir");
    let capture = capture_explore(root.path(), &[]);
    wince::assert_eq!(
        (capture.captured.text, capture.skipped),
        (String::new(), Vec::new())
    );
}

#[test]
fn a_binary_file_is_skipped_and_left_out() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(root.path().join("data.bin"), [1u8, 2, 0, 3]).expect("write binary");
    write(root.path(), "foo.txt", "alpha\n");

    let capture = capture_explore(root.path(), &["data.bin".into(), "foo.txt".into()]);

    // The text file is captured; the binary one is reported so a caller adding
    // it can refuse, and it never reaches the diff.
    wince::assert_eq!(
        (capture.captured.text, capture.skipped),
        (
            "\
diff --git a/foo.txt b/foo.txt
--- a/foo.txt
+++ b/foo.txt
@@ -1,1 +1,1 @@
 alpha
"
            .to_string(),
            vec![("data.bin".to_string(), SkipReason::Binary)]
        )
    );
}

#[test]
fn a_missing_file_is_skipped_and_left_out() {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), "foo.txt", "alpha\n");

    let capture = capture_explore(root.path(), &["gone.txt".into(), "foo.txt".into()]);

    // A vanished path drops out of the capture with its reason recorded; the
    // rest of the set is captured unaffected.
    wince::assert_eq!(
        (capture.captured.text, capture.skipped),
        (
            "\
diff --git a/foo.txt b/foo.txt
--- a/foo.txt
+++ b/foo.txt
@@ -1,1 +1,1 @@
 alpha
"
            .to_string(),
            vec![("gone.txt".to_string(), SkipReason::Missing)]
        )
    );
}

/// The reviewer who authors comments and runs refreshes in these tests.
fn wez() -> Author {
    Author {
        name: "wez".to_string(),
        kind: AuthorKind::Human,
    }
}

/// Create an explore session over `paths` under `root`, its v0 the all-context
/// capture of that set.
fn explore_session(root: &Path, data: &Path, paths: &[String]) -> SessionLog {
    let identity = ProjectIdentity {
        canonical: "demo".to_string(),
        repo_root: Some(root.to_path_buf()),
        scm: None,
    };
    let capture = capture_explore(root, paths);
    create_session(data, &identity, root, &capture.captured, None).expect("create session")
}

/// Attach an after-side comment to `line` of `file` and return its id.
fn comment_on(log: &mut SessionLog, file: &str, line: u32) -> Ulid {
    DraftComment {
        author: wez(),
        target: CommentTarget::Lines {
            file: file.to_string(),
            side: Side::After,
            start_line: LineNo::new(line).unwrap(),
            end_line: LineNo::new(line).unwrap(),
        },
        body: "note".to_string(),
        disposition: None,
    }
    .append(log, LockWait::Block)
    .expect("append comment")
    .id
}

/// The single folded comment for `log`.
fn only_comment(log: &SessionLog) -> CommentState {
    let mut comments = fold(&read_records(log.path()).unwrap()).unwrap().comments;
    wince::assert_eq!(comments.len(), 1);
    comments.pop().unwrap()
}

#[test]
fn editing_the_reviewed_line_relocates_its_comment() {
    let root = tempfile::tempdir().expect("root");
    let data = tempfile::tempdir().expect("data");
    write(root.path(), "foo.txt", "alpha\nbeta\ngamma\n");
    let mut log = explore_session(root.path(), data.path(), &["foo.txt".into()]);
    let id = comment_on(&mut log, "foo.txt", 2);

    // The commented line's own text is rewritten, then the file is recaptured.
    write(root.path(), "foo.txt", "alpha\nBETA CHANGED\ngamma\n");
    let capture = capture_explore(root.path(), &["foo.txt".into()]);
    let outcome = refresh_session(&mut log, &capture.captured, wez(), LockWait::Block)
        .expect("refresh")
        .expect("a new version");

    // A single all-context file whose commented line changed relocates the
    // comment through the shared base rather than declaring it outdated: the
    // comment is retained, moved onto the new version, and flagged relocated.
    let comment = only_comment(&log);
    wince::assert_eq!(
        (
            outcome.exact,
            outcome.approximate,
            outcome.relocated,
            outcome.outdated,
            comment.id,
            comment.version,
            comment.confidence,
        ),
        (
            0,
            0,
            1,
            0,
            id,
            VersionNumber(1),
            Some(Confidence::Relocated),
        )
    );
}

#[test]
fn an_unrelated_edit_shifts_the_comment_forward_exactly() {
    let root = tempfile::tempdir().expect("root");
    let data = tempfile::tempdir().expect("data");
    write(root.path(), "foo.txt", "alpha\nbeta\ngamma\n");
    let mut log = explore_session(root.path(), data.path(), &["foo.txt".into()]);
    let id = comment_on(&mut log, "foo.txt", 3);

    // A line inserted above the commented one moves gamma from 3 to 4.
    write(root.path(), "foo.txt", "inserted\nalpha\nbeta\ngamma\n");
    let capture = capture_explore(root.path(), &["foo.txt".into()]);
    let outcome = refresh_session(&mut log, &capture.captured, wez(), LockWait::Block)
        .expect("refresh")
        .expect("a new version");

    let comment = only_comment(&log);
    wince::assert_eq!(
        (
            outcome.exact,
            outcome.relocated,
            outcome.outdated,
            comment.id,
            comment.target,
            comment.confidence,
        ),
        (
            1,
            0,
            0,
            id,
            CommentTarget::Lines {
                file: "foo.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(4).unwrap(),
                end_line: LineNo::new(4).unwrap(),
            },
            Some(Confidence::Exact),
        )
    );
}

#[test]
fn deleting_a_reviewed_file_does_not_abort_the_refresh() {
    let root = tempfile::tempdir().expect("root");
    let data = tempfile::tempdir().expect("data");
    write(root.path(), "foo.txt", "alpha\nbeta\n");
    write(root.path(), "bar.txt", "one\ntwo\n");
    let mut log = explore_session(
        root.path(),
        data.path(),
        &["bar.txt".into(), "foo.txt".into()],
    );
    let id = comment_on(&mut log, "foo.txt", 1);

    // bar.txt vanishes from disk. Recapturing the stored set omits it; the
    // refresh still captures, leaving foo.txt's comment exact rather than
    // failing because one file is gone.
    std::fs::remove_file(root.path().join("bar.txt")).expect("remove bar");
    let capture = capture_explore(root.path(), &["bar.txt".into(), "foo.txt".into()]);
    let outcome = refresh_session(&mut log, &capture.captured, wez(), LockWait::Block)
        .expect("refresh")
        .expect("a new version");

    let comment = only_comment(&log);
    wince::assert_eq!(
        (
            capture.skipped,
            outcome.exact,
            outcome.outdated,
            comment.id,
            comment.confidence,
        ),
        (
            vec![("bar.txt".to_string(), SkipReason::Missing)],
            1,
            0,
            id,
            Some(Confidence::Exact),
        )
    );
}

#[test]
fn an_empty_explore_session_creates_a_zero_file_v0_and_widens() {
    use wiff_core::ReviewState;

    let root = tempfile::tempdir().expect("root");
    let data = tempfile::tempdir().expect("data");

    // A session created from an empty set opens with a real v0 that holds no
    // files, so later adds and comments find a version to work against.
    let mut log = explore_session(root.path(), data.path(), &[]);
    let path = log.path().to_path_buf();
    let empty = ReviewState::load(&path).expect("load empty");
    wince::assert_eq!(
        (
            empty.session.source.clone(),
            empty.versions.len(),
            empty.latest_version().map(|v| v.files.len()),
        ),
        (SourceKind::Explore, 1, Some(0))
    );

    // Adding a file widens the set to a new version over the one readable file.
    write(root.path(), "foo.txt", "alpha\n");
    let capture = capture_explore(root.path(), &["foo.txt".into()]);
    refresh_session(&mut log, &capture.captured, wez(), LockWait::Block)
        .expect("refresh")
        .expect("a new version");
    let widened = ReviewState::load(&path).expect("load widened");
    wince::assert_eq!(
        (
            widened.versions.len(),
            widened.latest_version().map(|v| v
                .files
                .iter()
                .map(|f| f.new_path.clone())
                .collect::<Vec<_>>()),
        ),
        (2, Some(vec!["foo.txt".to_string()]))
    );
}

#[test]
fn widening_the_set_then_commenting_anchors_on_the_new_version() {
    let root = tempfile::tempdir().expect("root");
    let data = tempfile::tempdir().expect("data");
    write(root.path(), "foo.txt", "alpha\nbeta\n");
    let mut log = explore_session(root.path(), data.path(), &["foo.txt".into()]);

    // A file is added to the set, then a comment is authored on it. The widen
    // and the comment take the lock in turn; the comment anchors against the
    // latest version at that point, which is the one the widen just wrote.
    write(root.path(), "new.txt", "one\ntwo\nthree\n");
    let outcome = widen_explore(
        &mut log,
        root.path(),
        &["new.txt".into()],
        wez(),
        LockWait::Block,
    )
    .expect("widen")
    .expect("a new version");
    let added = DraftComment {
        author: wez(),
        target: CommentTarget::Lines {
            file: "new.txt".to_string(),
            side: Side::After,
            start_line: LineNo::new(2).unwrap(),
            end_line: LineNo::new(2).unwrap(),
        },
        body: "on two".to_string(),
        disposition: None,
    }
    .append(&mut log, LockWait::Block)
    .expect("append comment");

    // The comment records the just-widened version and anchors to the line's own
    // content, so it is attached to a version whose file set contains the new
    // file rather than the pre-widen version that lacked it.
    let comment = only_comment(&log);
    wince::assert_eq!(
        (
            outcome.version,
            added.version,
            comment.version,
            comment.target,
            comment.anchor.map(|anchor| anchor.snippet),
        ),
        (
            VersionNumber(1),
            VersionNumber(1),
            VersionNumber(1),
            CommentTarget::Lines {
                file: "new.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(2).unwrap(),
                end_line: LineNo::new(2).unwrap(),
            },
            Some(vec!["two".to_string()]),
        )
    );
}

#[test]
fn re_widening_with_an_existing_file_captures_no_new_version() {
    let root = tempfile::tempdir().expect("root");
    let data = tempfile::tempdir().expect("data");
    write(root.path(), "foo.txt", "alpha\n");
    let mut log = explore_session(root.path(), data.path(), &["foo.txt".into()]);

    // Re-adding a file already under review yields byte-identical text, so the
    // hash guard captures nothing and the widen reports no change.
    let outcome = widen_explore(
        &mut log,
        root.path(),
        &["foo.txt".into()],
        wez(),
        LockWait::Block,
    )
    .expect("widen");
    wince::assert_eq!(outcome, None);
}

#[test]
fn widening_a_binary_file_fails_without_writing_a_version() {
    let root = tempfile::tempdir().expect("root");
    let data = tempfile::tempdir().expect("data");
    write(root.path(), "foo.txt", "alpha\n");
    let mut log = explore_session(root.path(), data.path(), &["foo.txt".into()]);
    std::fs::write(root.path().join("data.bin"), [0u8, 1, 2]).expect("write binary");

    // A requested binary path fails the widen; the session keeps its lone v0 with
    // no version written for the rejected add.
    let error = widen_explore(
        &mut log,
        root.path(),
        &["data.bin".into()],
        wez(),
        LockWait::Block,
    )
    .expect_err("binary rejected");
    let versions = fold(&read_records(log.path()).unwrap())
        .unwrap()
        .versions
        .len();
    wince::assert_eq!(
        (format!("{error}"), versions),
        (
            "cannot include data.bin in the review: binary, not text".to_string(),
            1
        )
    );
}
