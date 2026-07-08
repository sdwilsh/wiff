#![allow(missing_docs)]

use wiff_diff::line::LineNo;
use wiff_diff::model::{Diff, DiffLine, FileDiff, FileStatus, Hunk, LineKind};
use wiff_diff::parse::{ParseError, parse};

fn line(kind: LineKind, text: &str, old: Option<u32>, new: Option<u32>) -> DiffLine {
    DiffLine {
        kind,
        text: text.to_string(),
        old_lineno: old.and_then(LineNo::new),
        new_lineno: new.and_then(LineNo::new),
    }
}

#[test]
fn parses_a_git_modification() {
    let input = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@ fn main() {
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;
";
    let expected = Diff {
        files: vec![FileDiff {
            old_path: "src/main.rs".to_string(),
            new_path: "src/main.rs".to_string(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 3,
                new_start: 1,
                new_len: 3,
                section: Some("fn main() {".to_string()),
                lines: vec![
                    line(LineKind::Context, "let a = 1;", Some(1), Some(1)),
                    line(LineKind::Removed, "let b = 2;", Some(2), None),
                    line(LineKind::Added, "let b = 3;", None, Some(2)),
                    line(LineKind::Context, "let c = 4;", Some(3), Some(3)),
                ],
            }],
        }],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn parses_added_and_deleted_files() {
    let input = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+first
+second
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-only line
";
    let expected = Diff {
        files: vec![
            FileDiff {
                old_path: "new.txt".to_string(),
                new_path: "new.txt".to_string(),
                status: FileStatus::Added,
                hunks: vec![Hunk {
                    old_start: 0,
                    old_len: 0,
                    new_start: 1,
                    new_len: 2,
                    section: None,
                    lines: vec![
                        line(LineKind::Added, "first", None, Some(1)),
                        line(LineKind::Added, "second", None, Some(2)),
                    ],
                }],
            },
            FileDiff {
                old_path: "gone.txt".to_string(),
                new_path: "gone.txt".to_string(),
                status: FileStatus::Deleted,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_len: 1,
                    new_start: 0,
                    new_len: 0,
                    section: None,
                    lines: vec![line(LineKind::Removed, "only line", Some(1), None)],
                }],
            },
        ],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn parses_a_rename() {
    let input = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 90%
rename from old/name.rs
rename to new/name.rs
--- a/old/name.rs
+++ b/new/name.rs
@@ -1 +1 @@
-was here
+is here
";
    let expected = Diff {
        files: vec![FileDiff {
            old_path: "old/name.rs".to_string(),
            new_path: "new/name.rs".to_string(),
            status: FileStatus::Renamed,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 1,
                section: None,
                lines: vec![
                    line(LineKind::Removed, "was here", Some(1), None),
                    line(LineKind::Added, "is here", None, Some(1)),
                ],
            }],
        }],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn parses_multiple_hunks_in_one_file() {
    let input = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 one
-two
+TWO
@@ -10,2 +10,2 @@
 ten
-eleven
+ELEVEN
";
    let expected = Diff {
        files: vec![FileDiff {
            old_path: "a.txt".to_string(),
            new_path: "a.txt".to_string(),
            status: FileStatus::Modified,
            hunks: vec![
                Hunk {
                    old_start: 1,
                    old_len: 2,
                    new_start: 1,
                    new_len: 2,
                    section: None,
                    lines: vec![
                        line(LineKind::Context, "one", Some(1), Some(1)),
                        line(LineKind::Removed, "two", Some(2), None),
                        line(LineKind::Added, "TWO", None, Some(2)),
                    ],
                },
                Hunk {
                    old_start: 10,
                    old_len: 2,
                    new_start: 10,
                    new_len: 2,
                    section: None,
                    lines: vec![
                        line(LineKind::Context, "ten", Some(10), Some(10)),
                        line(LineKind::Removed, "eleven", Some(11), None),
                        line(LineKind::Added, "ELEVEN", None, Some(11)),
                    ],
                },
            ],
        }],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn parses_plain_diff_u_with_timestamp_tabs() {
    // `diff -u` output has no `diff --git` line and appends a tab and timestamp
    // to each path header, with no `a/`/`b/` prefixes.
    let input = "\
--- foo.txt	2026-01-01 00:00:00.000000000 +0000
+++ foo.txt	2026-01-02 00:00:00.000000000 +0000
@@ -1,2 +1,2 @@
 keep
-old
+new
";
    let expected = Diff {
        files: vec![FileDiff {
            old_path: "foo.txt".to_string(),
            new_path: "foo.txt".to_string(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 2,
                new_start: 1,
                new_len: 2,
                section: None,
                lines: vec![
                    line(LineKind::Context, "keep", Some(1), Some(1)),
                    line(LineKind::Removed, "old", Some(2), None),
                    line(LineKind::Added, "new", None, Some(2)),
                ],
            }],
        }],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn ignores_no_newline_at_end_of_file_marker() {
    let input = "\
diff --git a/n.txt b/n.txt
--- a/n.txt
+++ b/n.txt
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
    let expected = Diff {
        files: vec![FileDiff {
            old_path: "n.txt".to_string(),
            new_path: "n.txt".to_string(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 1,
                section: None,
                lines: vec![
                    line(LineKind::Removed, "old", Some(1), None),
                    line(LineKind::Added, "new", None, Some(1)),
                ],
            }],
        }],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn parses_empty_context_line_and_defaulted_hunk_lengths() {
    // A blank body line is an empty context line, and a hunk header may omit
    // the length, which defaults to 1.
    let input = "\
diff --git a/e.txt b/e.txt
--- a/e.txt
+++ b/e.txt
@@ -1,3 +1,3 @@
 first

-third
+THIRD
";
    let expected = Diff {
        files: vec![FileDiff {
            old_path: "e.txt".to_string(),
            new_path: "e.txt".to_string(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 3,
                new_start: 1,
                new_len: 3,
                section: None,
                lines: vec![
                    line(LineKind::Context, "first", Some(1), Some(1)),
                    line(LineKind::Context, "", Some(2), Some(2)),
                    line(LineKind::Removed, "third", Some(3), None),
                    line(LineKind::Added, "THIRD", None, Some(3)),
                ],
            }],
        }],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn hunk_header_without_lengths_defaults_to_one() {
    let input = "\
diff --git a/s.txt b/s.txt
--- a/s.txt
+++ b/s.txt
@@ -5 +5 @@
-was
+now
";
    let expected = Diff {
        files: vec![FileDiff {
            old_path: "s.txt".to_string(),
            new_path: "s.txt".to_string(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 5,
                old_len: 1,
                new_start: 5,
                new_len: 1,
                section: None,
                lines: vec![
                    line(LineKind::Removed, "was", Some(5), None),
                    line(LineKind::Added, "now", None, Some(5)),
                ],
            }],
        }],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn tolerates_surrounding_prose_before_the_diff() {
    let input = "\
Here is a diff you should review:

diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-a
+b
";
    let expected = Diff {
        files: vec![FileDiff {
            old_path: "a.txt".to_string(),
            new_path: "a.txt".to_string(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 1,
                section: None,
                lines: vec![
                    line(LineKind::Removed, "a", Some(1), None),
                    line(LineKind::Added, "b", None, Some(1)),
                ],
            }],
        }],
    };
    k9::assert_equal!(parse(input).unwrap(), expected);
}

#[test]
fn empty_input_yields_no_files() {
    k9::assert_equal!(parse("").unwrap(), Diff { files: vec![] });
}

#[test]
fn rejects_a_malformed_hunk_header() {
    let input = "\
diff --git a/x.txt b/x.txt
--- a/x.txt
+++ b/x.txt
@@ this is not a range @@
";
    k9::assert_equal!(
        parse(input),
        Err(ParseError::BadHunkHeader {
            line: 4,
            content: "@@ this is not a range @@".to_string(),
        })
    );
}

#[test]
fn rejects_an_unrecognized_line_inside_a_hunk() {
    let input = "\
diff --git a/x.txt b/x.txt
--- a/x.txt
+++ b/x.txt
@@ -1 +1 @@
-a
!garbage
";
    k9::assert_equal!(
        parse(input),
        Err(ParseError::OrphanLine {
            line: 6,
            content: "!garbage".to_string(),
        })
    );
}
