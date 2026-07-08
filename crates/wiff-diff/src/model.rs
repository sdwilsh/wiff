//! The in-memory model of a parsed unified diff.
//!
//! A [`Diff`] is a set of [`FileDiff`]s, each a list of [`Hunk`]s of tagged
//! [`DiffLine`]s. The model is the shared vocabulary for parsing, rendering,
//! reconstitution, and rebasing, so it is deliberately plain data.

use serde::{Deserialize, Serialize};

use crate::line::LineNo;

/// Which side of a change a line or anchor belongs to: the content before the
/// change (removed and context lines) or after it (added and context lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The pre-change content.
    Before,
    /// The post-change content.
    After,
}

/// How a file was changed between the two sides of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// The file exists only on the after side.
    Added,
    /// The file exists only on the before side.
    Deleted,
    /// The file exists on both sides with changed content.
    Modified,
    /// The file was renamed (and possibly also modified).
    Renamed,
}

/// The role of a single line within a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    /// Present unchanged on both sides.
    Context,
    /// Present only on the after side.
    Added,
    /// Present only on the before side.
    Removed,
}

/// One line of a hunk: its role and text, plus its 1-based line numbers on
/// whichever sides it appears on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    /// The line's role in the change.
    pub kind: LineKind,
    /// The line's text, without its trailing newline.
    pub text: String,
    /// The line number on the before side, for context and removed lines.
    pub old_lineno: Option<LineNo>,
    /// The line number on the after side, for context and added lines.
    pub new_lineno: Option<LineNo>,
}

impl DiffLine {
    /// Whether this line appears on `side`.
    pub fn on_side(&self, side: Side) -> bool {
        matches!(
            (side, self.kind),
            (Side::Before, LineKind::Context | LineKind::Removed)
                | (Side::After, LineKind::Context | LineKind::Added)
        )
    }

    /// This line's number on `side`, if it appears there.
    pub fn lineno(&self, side: Side) -> Option<LineNo> {
        match side {
            Side::Before => self.old_lineno,
            Side::After => self.new_lineno,
        }
    }
}

/// A contiguous run of changes, corresponding to one `@@ ... @@` group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// The 1-based starting line on the before side.
    pub old_start: u32,
    /// The number of before-side lines the hunk covers.
    pub old_len: u32,
    /// The 1-based starting line on the after side.
    pub new_start: u32,
    /// The number of after-side lines the hunk covers.
    pub new_len: u32,
    /// The section heading trailing the `@@` marker, if any (e.g. the enclosing
    /// function), without a leading space.
    pub section: Option<String>,
    /// The hunk's lines in order.
    pub lines: Vec<DiffLine>,
}

/// The changes to a single file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// The path on the before side (equal to `new_path` for a plain edit).
    pub old_path: String,
    /// The path on the after side.
    pub new_path: String,
    /// How the file changed.
    pub status: FileStatus,
    /// The file's hunks in order.
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The path to show for the file: the after path unless the file was
    /// deleted, in which case the before path.
    pub fn display_path(&self) -> &str {
        match self.status {
            FileStatus::Deleted => &self.old_path,
            _ => &self.new_path,
        }
    }
}

/// A parsed unified diff: the files it touches, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    /// The changed files in the order they appear in the diff.
    pub files: Vec<FileDiff>,
}
