//! Building unified diff text from whole-file contents rather than from a
//! repository. A repo-less pull has no local objects to diff, so wiff fetches
//! each changed file's base and head contents over the forge API and assembles
//! the diff here. Feeding similar the full contents gives the same wide context
//! and whole-file view as an in-repo capture, instead of the fixed few lines a
//! forge's ready-made diff returns.

use std::fmt::Write;

use similar::TextDiff;
use wiff_diff::FileStatus;

/// One changed file of a repo-less pull request, with the contents needed to
/// render its portion of the unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// How the file changed between the base and the head.
    pub status: FileStatus,
    /// The path on the base side. Equal to `new_path` except for a rename.
    pub old_path: String,
    /// The path on the head side. Equal to `old_path` except for a rename.
    pub new_path: String,
    /// The file's contents on each side, or a marker that it is binary.
    pub content: Content,
}

/// A changed file's contents across the two sides of the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// The full text of each side.
    Text {
        /// The base-side text, empty for a pure add.
        before: String,
        /// The head-side text, empty for a pure delete.
        after: String,
    },
    /// A binary file, rendered as the hunkless note git emits for one.
    Binary,
}

/// Assemble git-format unified diff text from the whole-file contents of each
/// changed file, for use when there is no local repository to diff against.
pub fn assemble_diff(files: &[ChangedFile]) -> String {
    let mut out = String::new();
    for file in files {
        render_file(&mut out, file).expect("formatting into a String is infallible");
    }
    out
}

/// Append one file's section to `out`.
fn render_file(out: &mut String, file: &ChangedFile) -> std::fmt::Result {
    let old = &file.old_path;
    let new = &file.new_path;
    writeln!(out, "diff --git a/{old} b/{new}")?;
    // An add or delete needs no mode line: the parser reads that status from the
    // `/dev/null` side of the path lines below, and the forge diff has no file
    // mode wiff could report faithfully.
    match file.status {
        FileStatus::Renamed => {
            writeln!(out, "rename from {old}")?;
            writeln!(out, "rename to {new}")?;
        }
        FileStatus::Added | FileStatus::Deleted | FileStatus::Modified => {}
    }
    render_path_headers(out, file.status, old, new)?;
    match &file.content {
        Content::Binary => render_binary(out, file.status, old, new)?,
        Content::Text { before, after } => render_text(out, before, after)?,
    }
    Ok(())
}

/// Append the `---`/`+++` path lines, using `/dev/null` for the absent side of a
/// pure add or delete. Emitted for binary files too, not only text, because a
/// path containing a space cannot be recovered from the `diff --git` line alone.
fn render_path_headers(
    out: &mut String,
    status: FileStatus,
    old: &str,
    new: &str,
) -> std::fmt::Result {
    match status {
        FileStatus::Added => writeln!(out, "--- /dev/null")?,
        _ => writeln!(out, "--- a/{old}")?,
    }
    match status {
        FileStatus::Deleted => writeln!(out, "+++ /dev/null")?,
        _ => writeln!(out, "+++ b/{new}")?,
    }
    Ok(())
}

/// Append the hunks similar computes from the two sides. The context radius
/// covers the whole file, so a changed file shows in full rather than in
/// isolated hunks, matching an in-repo capture's wide window.
fn render_text(out: &mut String, before: &str, after: &str) -> std::fmt::Result {
    let radius = before.lines().count().max(after.lines().count());
    let diff = TextDiff::from_lines(before, after);
    for hunk in diff.unified_diff().context_radius(radius).iter_hunks() {
        write!(out, "{hunk}")?;
    }
    Ok(())
}

/// Append the binary note git renders in place of hunks, naming `/dev/null` for
/// the absent side of a pure add or delete.
fn render_binary(out: &mut String, status: FileStatus, old: &str, new: &str) -> std::fmt::Result {
    let (left, right) = match status {
        FileStatus::Added => ("/dev/null".to_string(), format!("b/{new}")),
        FileStatus::Deleted => (format!("a/{old}"), "/dev/null".to_string()),
        _ => (format!("a/{old}"), format!("b/{new}")),
    };
    writeln!(out, "Binary files {left} and {right} differ")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use wiff_diff::{LineKind, parse};

    use super::*;

    /// One changed file of each kind, exercising every header and content path
    /// the assembler emits.
    fn every_change_kind() -> Vec<ChangedFile> {
        vec![
            ChangedFile {
                status: FileStatus::Added,
                old_path: "new.txt".to_string(),
                new_path: "new.txt".to_string(),
                content: Content::Text {
                    before: String::new(),
                    after: "one\ntwo\n".to_string(),
                },
            },
            ChangedFile {
                status: FileStatus::Modified,
                old_path: "mod.txt".to_string(),
                new_path: "mod.txt".to_string(),
                content: Content::Text {
                    before: "alpha\nbeta\ngamma\n".to_string(),
                    after: "alpha\nBETA\ngamma\n".to_string(),
                },
            },
            ChangedFile {
                status: FileStatus::Deleted,
                old_path: "old.txt".to_string(),
                new_path: "old.txt".to_string(),
                content: Content::Text {
                    before: "gone\n".to_string(),
                    after: String::new(),
                },
            },
            ChangedFile {
                status: FileStatus::Renamed,
                old_path: "from.txt".to_string(),
                new_path: "to.txt".to_string(),
                content: Content::Text {
                    before: "keep\nchange\n".to_string(),
                    after: "keep\nchanged\n".to_string(),
                },
            },
            ChangedFile {
                status: FileStatus::Modified,
                old_path: "img.png".to_string(),
                new_path: "img.png".to_string(),
                content: Content::Binary,
            },
        ]
    }

    #[test]
    fn assembles_git_shaped_text_for_each_change_kind() {
        wince::assert_eq!(
            assemble_diff(&every_change_kind()),
            "\
diff --git a/new.txt b/new.txt
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
diff --git a/mod.txt b/mod.txt
--- a/mod.txt
+++ b/mod.txt
@@ -1,3 +1,3 @@
 alpha
-beta
+BETA
 gamma
diff --git a/old.txt b/old.txt
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-gone
diff --git a/from.txt b/to.txt
rename from from.txt
rename to to.txt
--- a/from.txt
+++ b/to.txt
@@ -1,2 +1,2 @@
 keep
-change
+changed
diff --git a/img.png b/img.png
--- a/img.png
+++ b/img.png
Binary files a/img.png and b/img.png differ
"
        );
    }

    /// Render a parsed diff back to a compact form for a full-value assertion:
    /// each file's status and paths, then its hunk lines with a marker.
    fn dump(text: &str) -> String {
        let diff = parse(text).expect("assembled text parses");
        let mut out = String::new();
        for file in &diff.files {
            out.push_str(&format!(
                "{:?} {} -> {}\n",
                file.status, file.old_path, file.new_path
            ));
            for hunk in &file.hunks {
                for line in &hunk.lines {
                    let marker = match line.kind {
                        LineKind::Context => ' ',
                        LineKind::Added => '+',
                        LineKind::Removed => '-',
                    };
                    out.push_str(&format!("{marker}{}\n", line.text));
                }
            }
        }
        out
    }

    #[test]
    fn assembled_text_parses_back_to_the_expected_model() {
        wince::assert_eq!(
            dump(&assemble_diff(&every_change_kind())),
            "\
Added new.txt -> new.txt
+one
+two
Modified mod.txt -> mod.txt
 alpha
-beta
+BETA
 gamma
Deleted old.txt -> old.txt
-gone
Renamed from.txt -> to.txt
 keep
-change
+changed
Modified img.png -> img.png
"
        );
    }

    #[test]
    fn assembles_a_file_whose_content_lacks_a_trailing_newline() {
        let files = vec![ChangedFile {
            status: FileStatus::Added,
            old_path: "solo.txt".to_string(),
            new_path: "solo.txt".to_string(),
            content: Content::Text {
                before: String::new(),
                after: "solo".to_string(),
            },
        }];
        let text = assemble_diff(&files);
        // similar marks the unterminated final line; the parser drops the marker
        // and reads the line as a plain addition.
        wince::assert_eq!(
            text,
            "\
diff --git a/solo.txt b/solo.txt
--- /dev/null
+++ b/solo.txt
@@ -0,0 +1 @@
+solo
\\ No newline at end of file
"
        );
        wince::assert_eq!(dump(&text), "Added solo.txt -> solo.txt\n+solo\n");
    }

    #[test]
    fn assembles_a_pure_rename_with_no_content_change() {
        let files = vec![ChangedFile {
            status: FileStatus::Renamed,
            old_path: "a.txt".to_string(),
            new_path: "b.txt".to_string(),
            content: Content::Text {
                before: "same\n".to_string(),
                after: "same\n".to_string(),
            },
        }];
        let text = assemble_diff(&files);
        // Equal sides yield no hunk; the rename headers alone record the move.
        wince::assert_eq!(
            text,
            "\
diff --git a/a.txt b/b.txt
rename from a.txt
rename to b.txt
--- a/a.txt
+++ b/b.txt
"
        );
        wince::assert_eq!(dump(&text), "Renamed a.txt -> b.txt\n");
    }

    #[test]
    fn assembles_binary_add_and_delete() {
        let files = vec![
            ChangedFile {
                status: FileStatus::Added,
                old_path: "added.png".to_string(),
                new_path: "added.png".to_string(),
                content: Content::Binary,
            },
            ChangedFile {
                status: FileStatus::Deleted,
                old_path: "gone.png".to_string(),
                new_path: "gone.png".to_string(),
                content: Content::Binary,
            },
        ];
        let text = assemble_diff(&files);
        wince::assert_eq!(
            text,
            "\
diff --git a/added.png b/added.png
--- /dev/null
+++ b/added.png
Binary files /dev/null and b/added.png differ
diff --git a/gone.png b/gone.png
--- a/gone.png
+++ /dev/null
Binary files a/gone.png and /dev/null differ
"
        );
        // With no hunks, the parser derives each status from the `/dev/null`
        // side of the path lines alone.
        wince::assert_eq!(
            dump(&text),
            "Added added.png -> added.png\nDeleted gone.png -> gone.png\n"
        );
    }
}
