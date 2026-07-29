//! Synthesizing an all-context diff over a fixed set of files.
//!
//! An explore review annotates existing code rather than a change. Each file is
//! captured as a hunk whose every line is context (identical before and after),
//! read at its working-copy state, so a comment anchors to real code with no
//! change implied. The reviewed file set is the set of paths captured here;
//! adding a file recaptures the widened set, and a refresh re-reads the same set
//! from disk.
//!
//! The synthesized text is LF-normalized rather than byte-faithful: each line is
//! re-emitted with a single trailing newline, so a CRLF file loses its carriage
//! returns and a file with no final newline gains one. The reviewer annotates
//! this normalized view, and a file that differs from a prior capture only in
//! its line endings hashes identically and captures no new version.

use std::path::Path;

use wiff_diff::decode_text;

use crate::record::SourceKind;
use crate::source::CapturedDiff;

/// Why a requested path was left out of an all-context capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The path does not exist or could not be read.
    Missing,
    /// The path holds binary (non-text) content.
    Binary,
}

impl SkipReason {
    /// A short reason phrase for a message naming the skipped path.
    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::Missing => "not found or unreadable",
            SkipReason::Binary => "binary, not text",
        }
    }
}

/// An all-context capture of a set of files, plus the paths it could not include
/// and why.
pub struct ExploreCapture {
    /// The synthesized all-context diff over the readable text files.
    pub captured: CapturedDiff,
    /// The requested paths left out, each with its reason, in path order.
    pub skipped: Vec<(String, SkipReason)>,
}

/// Read each of `paths` under `root` and synthesize an all-context diff over the
/// readable text files. `paths` are repository-relative; they are sorted and
/// deduplicated first, so the same set yields byte-identical text whatever order
/// it is given in.
pub fn capture_explore(root: &Path, paths: &[String]) -> ExploreCapture {
    let mut ordered: Vec<&String> = paths.iter().collect();
    ordered.sort();
    ordered.dedup();

    let mut text = String::new();
    let mut skipped = Vec::new();
    for path in ordered {
        match read_text_file(root, path) {
            FileRead::Text(content) => append_file_diff(&mut text, path, &content),
            FileRead::Binary => skipped.push((path.clone(), SkipReason::Binary)),
            FileRead::Missing => skipped.push((path.clone(), SkipReason::Missing)),
        }
    }
    ExploreCapture {
        captured: CapturedDiff {
            text,
            source: SourceKind::Explore,
            base_revision: None,
            base_tip_relative: false,
            head_revision: None,
        },
        skipped,
    }
}

/// The outcome of reading one file for an all-context capture.
enum FileRead {
    /// A text file with its whole content.
    Text(String),
    /// The path holds binary content.
    Binary,
    /// The path does not exist or could not be read.
    Missing,
}

/// Read `path` under `root`, classifying it for an all-context capture. Content
/// that is not text is treated as binary, since the diff synthesis and the
/// anchoring model are line-oriented text.
fn read_text_file(root: &Path, path: &str) -> FileRead {
    let bytes = match std::fs::read(root.join(path)) {
        Ok(bytes) => bytes,
        Err(_) => return FileRead::Missing,
    };
    match decode_text(bytes) {
        Some(text) => FileRead::Text(text),
        None => FileRead::Binary,
    }
}

/// Append `content` to `out` as one all-context file diff: git-style headers
/// followed by a single hunk whose every line is context. An empty file emits
/// its headers with no hunk, matching a zero-line file's absence of content.
fn append_file_diff(out: &mut String, path: &str, content: &str) {
    out.push_str(&format!("diff --git a/{path} b/{path}\n"));
    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return;
    }
    let count = lines.len();
    out.push_str(&format!("@@ -1,{count} +1,{count} @@\n"));
    // Each line is re-emitted with a leading context space and a trailing
    // newline: the content bytes minus its own newlines, plus two per line.
    out.reserve(content.len() + count);
    for line in lines {
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
}
