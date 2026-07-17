//! Presenting the review as the change between an earlier diff version and the
//! latest one, rather than against the baseline.
//!
//! The review always shows the latest version's after content on the right. The
//! left side is ordinarily the latest version's before side (the captured diff,
//! unchanged), but a reviewer can choose an earlier version as the reference
//! point to see only what has changed since they last looked. That comparison is
//! synthesized here: reconstruct each file's content at both points and diff it
//! with `similar`, producing an ordinary [`Diff`] the renderer already knows how
//! to show.
//!
//! Because the right side stays pinned to the latest version, a comment on an
//! after-side line sits exactly where it would in the baseline view. A comment
//! on a left-side line is authored against whatever version that content came
//! from, which [`Comparison::before_origin`] records per file so the caller can
//! anchor it correctly.

use std::collections::HashMap;

use similar::{DiffTag, TextDiff};
use wiff_diff::reconstitute::known_lines;
use wiff_diff::{Diff, DiffLine, FileDiff, FileStatus, Hunk, LineKind, LineNo, Side};

/// The version and side a run of presented lines was reconstructed from, so a
/// comment placed on them can be anchored against the version it truly belongs
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineOrigin {
    /// The diff version the content came from.
    pub version: u32,
    /// The side of that version's diff the content came from.
    pub side: Side,
}

/// A synthesized "changes since an earlier version" diff, with the provenance a
/// caller needs to anchor comments made against it.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// The synthesized diff: for each changed file, its content at the reference
    /// version on the left and at the latest version on the right.
    pub diff: Diff,
    /// The version and side every after-side line represents; always the latest
    /// version's after side.
    pub after_origin: LineOrigin,
    /// Per file (keyed by display path), the version and side its before-side
    /// lines represent.
    pub before_origin: HashMap<String, LineOrigin>,
}

/// Synthesize the changes between the `from` version's after content and the
/// `latest` version's after content, for every file that differs between them.
///
/// The left side of each file is the file's after content at `from` when that
/// file was part of the `from` version's diff; otherwise the file was unchanged
/// then, so its content is taken from the latest version's before side (the
/// shared baseline). Files present at `from` but no longer in the latest diff do
/// not appear: the presented right side always follows the latest version.
pub fn compare_versions(
    latest: &Diff,
    latest_version: u32,
    from: &Diff,
    from_version: u32,
) -> Comparison {
    let mut files = Vec::new();
    let mut before_origin = HashMap::new();
    for latest_file in &latest.files {
        let path = latest_file.display_path().to_string();
        let after = known_lines(latest_file, Side::After);
        let (before, origin) = match from.files.iter().find(|f| f.display_path() == path) {
            Some(from_file) => (
                known_lines(from_file, Side::After),
                LineOrigin {
                    version: from_version,
                    side: Side::After,
                },
            ),
            None => (
                known_lines(latest_file, Side::Before),
                LineOrigin {
                    version: latest_version,
                    side: Side::Before,
                },
            ),
        };
        let Some(file) = synthesize_file(
            latest_file.old_path.clone(),
            latest_file.new_path.clone(),
            &before,
            &after,
        ) else {
            continue;
        };
        before_origin.insert(path, origin);
        files.push(file);
    }
    Comparison {
        diff: Diff { files },
        after_origin: LineOrigin {
            version: latest_version,
            side: Side::After,
        },
        before_origin,
    }
}

/// Diff the `before` and `after` content of one file into a single-hunk
/// [`FileDiff`] spanning the whole file, keeping each line's real number on its
/// side. Returns `None` when the two sides are identical, so an unchanged file
/// is left out of the comparison entirely.
fn synthesize_file(
    old_path: String,
    new_path: String,
    before: &[(LineNo, String)],
    after: &[(LineNo, String)],
) -> Option<FileDiff> {
    let before_texts: Vec<&str> = before.iter().map(|(_, t)| t.as_str()).collect();
    let after_texts: Vec<&str> = after.iter().map(|(_, t)| t.as_str()).collect();
    let text_diff = TextDiff::from_slices(&before_texts, &after_texts);

    let mut lines = Vec::new();
    let mut changed = false;
    for op in text_diff.ops() {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => {
                for (offset, old_index) in old_range.clone().enumerate() {
                    lines.push(DiffLine {
                        kind: LineKind::Context,
                        text: before[old_index].1.clone(),
                        old_lineno: Some(before[old_index].0),
                        new_lineno: Some(after[new_range.start + offset].0),
                    });
                }
            }
            DiffTag::Delete | DiffTag::Replace => {
                changed = true;
                for old_index in old_range.clone() {
                    lines.push(DiffLine {
                        kind: LineKind::Removed,
                        text: before[old_index].1.clone(),
                        old_lineno: Some(before[old_index].0),
                        new_lineno: None,
                    });
                }
                for new_index in new_range.clone() {
                    lines.push(DiffLine {
                        kind: LineKind::Added,
                        text: after[new_index].1.clone(),
                        old_lineno: None,
                        new_lineno: Some(after[new_index].0),
                    });
                }
            }
            DiffTag::Insert => {
                changed = true;
                for new_index in new_range.clone() {
                    lines.push(DiffLine {
                        kind: LineKind::Added,
                        text: after[new_index].1.clone(),
                        old_lineno: None,
                        new_lineno: Some(after[new_index].0),
                    });
                }
            }
        }
    }
    if !changed {
        return None;
    }

    let old_len = before.len() as u32;
    let new_len = after.len() as u32;
    let status = match (old_len, new_len) {
        (0, _) => FileStatus::Added,
        (_, 0) => FileStatus::Deleted,
        _ if old_path != new_path => FileStatus::Renamed,
        _ => FileStatus::Modified,
    };
    Some(FileDiff {
        old_path,
        new_path,
        status,
        hunks: vec![Hunk {
            old_start: before.first().map_or(1, |(n, _)| n.get()),
            old_len,
            new_start: after.first().map_or(1, |(n, _)| n.get()),
            new_len,
            section: None,
            lines,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::{LineOrigin, compare_versions, synthesize_file};
    use std::collections::HashMap;
    use wiff_diff::{Diff, FileStatus, LineKind, LineNo, Side, parse};

    /// Number `texts` from 1 as `synthesize_file` expects its side slices.
    fn numbered(texts: &[&str]) -> Vec<(LineNo, String)> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                (
                    LineNo::new(i as u32 + 1).expect("line numbers start at 1"),
                    (*t).to_string(),
                )
            })
            .collect()
    }

    /// Render `diff` back to a compact unified form for full-value assertions:
    /// each file's status and path, then its hunk lines with a `+`/`-`/` `
    /// marker and both line numbers.
    fn dump(diff: &Diff) -> String {
        let mut out = String::new();
        for file in &diff.files {
            out.push_str(&format!(
                "{:?} {} -> {}\n",
                file.status, file.old_path, file.new_path
            ));
            for hunk in &file.hunks {
                out.push_str(&format!(
                    "@@ -{},{} +{},{} @@\n",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ));
                for line in &hunk.lines {
                    let marker = match line.kind {
                        LineKind::Context => ' ',
                        LineKind::Added => '+',
                        LineKind::Removed => '-',
                    };
                    let old = line.old_lineno.map_or("-".to_string(), |n| n.to_string());
                    let new = line.new_lineno.map_or("-".to_string(), |n| n.to_string());
                    out.push_str(&format!("{old:>2} {new:>2} {marker}{}\n", line.text));
                }
            }
        }
        out
    }

    /// The changes since `from` for a file edited across two versions: the left
    /// side is the file as it stood at `from`, the right side the latest, and a
    /// line unchanged between them is context. The before origin points at the
    /// reference version's after side.
    #[test]
    fn compares_a_file_edited_between_two_versions() {
        // v0 added a three-line file; v1 rewrote its middle line and appended a
        // fourth. The comparison of v0 against v1 shows only that later change.
        let v0 = parse(
            "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,3 @@
+alpha
+beta
+gamma
",
        )
        .unwrap();
        let v1 = parse(
            "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+alpha
+BETA
+gamma
+delta
",
        )
        .unwrap();

        let comparison = compare_versions(&v1, 1, &v0, 0);
        let expected_before: HashMap<String, LineOrigin> = [(
            "f.txt".to_string(),
            LineOrigin {
                version: 0,
                side: Side::After,
            },
        )]
        .into_iter()
        .collect();
        wince::assert_eq!(
            comparison.after_origin,
            LineOrigin {
                version: 1,
                side: Side::After,
            }
        );
        wince::assert_eq!(comparison.before_origin, expected_before);
        let expected = "\
Modified f.txt -> f.txt
@@ -1,3 +1,4 @@
 1  1  alpha
 2  - -beta
 -  2 +BETA
 3  3  gamma
 -  4 +delta
";
        wince::assert_eq!(dump(&comparison.diff), expected.to_string());
    }

    /// A file that only entered the diff after the reference version has no
    /// content at `from`, so its left side falls back to the latest version's
    /// before side (the shared baseline) and it reads as changed since then.
    #[test]
    fn falls_back_to_the_baseline_for_a_file_absent_at_the_reference() {
        // v0 touched only a.txt. v1 also modifies b.txt (against a committed
        // base). Comparing v0 against v1, b.txt was unchanged at v0, so its left
        // side is v1's before side.
        let v0 = parse(
            "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 one
-two
+TWO
",
        )
        .unwrap();
        let v1 = parse(
            "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 one
-two
+TWO
diff --git a/b.txt b/b.txt
--- a/b.txt
+++ b/b.txt
@@ -1,2 +1,2 @@
 red
-green
+GREEN
",
        )
        .unwrap();

        let comparison = compare_versions(&v1, 1, &v0, 0);
        // a.txt is unchanged between v0 and v1, so it drops out; only b.txt,
        // whose left side comes from the baseline, remains.
        let expected_before: HashMap<String, LineOrigin> = [(
            "b.txt".to_string(),
            LineOrigin {
                version: 1,
                side: Side::Before,
            },
        )]
        .into_iter()
        .collect();
        wince::assert_eq!(comparison.before_origin, expected_before);
        let expected = "\
Modified b.txt -> b.txt
@@ -1,2 +1,2 @@
 1  1  red
 2  - -green
 -  2 +GREEN
";
        wince::assert_eq!(dump(&comparison.diff), expected.to_string());
    }

    /// `synthesize_file` classifies status from the before/after line counts
    /// first and only falls through to `Renamed` when both sides have content
    /// and the paths differ. Because `parse` now mirrors the `/dev/null` side
    /// of an add or delete, such a file arrives with equal paths and cannot
    /// reach that fall-through; this pins each arm so a blank-path regression
    /// upstream would show up as a spurious rename here.
    #[test]
    fn synthesize_file_classifies_status_from_content_then_paths() {
        let classify = |old: &str, new: &str, before: &[&str], after: &[&str]| {
            synthesize_file(
                old.to_string(),
                new.to_string(),
                &numbered(before),
                &numbered(after),
            )
            .map(|file| file.status)
        };
        // An add and a delete keep their status even though the mirrored paths
        // are equal on both sides.
        wince::assert_eq!(
            classify("f.txt", "f.txt", &[], &["hello"]),
            Some(FileStatus::Added)
        );
        wince::assert_eq!(
            classify("f.txt", "f.txt", &["bye"], &[]),
            Some(FileStatus::Deleted)
        );
        // Equal paths with content on both sides is a plain edit, never a rename.
        wince::assert_eq!(
            classify("f.txt", "f.txt", &["old"], &["new"]),
            Some(FileStatus::Modified)
        );
        // Only genuinely differing paths yield a rename.
        wince::assert_eq!(
            classify("old.txt", "new.txt", &["old"], &["new"]),
            Some(FileStatus::Renamed)
        );
    }
}
