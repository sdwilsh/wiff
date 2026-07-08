//! Parsing unified diff text into the [`Diff`] model.
//!
//! Handles both `git diff` output (with its `diff --git` and extended headers)
//! and plain `diff -u` output. Unrecognized leading lines are tolerated so a
//! diff embedded in surrounding text still parses.

use crate::line::LineNo;
use crate::model::{Diff, DiffLine, FileDiff, FileStatus, Hunk, LineKind};

/// An error encountered while parsing a unified diff.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    /// A hunk header (`@@ -a,b +c,d @@`) was malformed.
    #[error("malformed hunk header on line {line}: {content:?}")]
    BadHunkHeader {
        /// The 1-based line number in the input.
        line: usize,
        /// The offending line's content.
        content: String,
    },
    /// A hunk body line appeared before any hunk header.
    #[error("diff content on line {line} is outside any hunk: {content:?}")]
    OrphanLine {
        /// The 1-based line number in the input.
        line: usize,
        /// The offending line's content.
        content: String,
    },
}

/// Parse unified diff `text` into a [`Diff`].
pub fn parse(text: &str) -> Result<Diff, ParseError> {
    let mut parser = Parser::default();
    for (index, raw) in text.lines().enumerate() {
        parser.feed(index + 1, raw)?;
    }
    parser.finish();
    Ok(Diff {
        files: parser.files,
    })
}

/// The path named by a `---` or `+++` header, mapping `/dev/null` and stripping
/// the conventional `a/` or `b/` prefix.
fn header_path(rest: &str) -> Option<String> {
    // Git appends a tab and metadata to paths containing spaces; drop it.
    let path = rest.split('\t').next().unwrap_or(rest).trim();
    if path == "/dev/null" {
        return None;
    }
    let unprefixed = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    Some(unprefixed.to_string())
}

#[derive(Default)]
struct Parser {
    files: Vec<FileDiff>,
    current: Option<FileDiff>,
    hunk: Option<Hunk>,
    old_lineno: u32,
    new_lineno: u32,
    // Lines still expected on each side of the open hunk, per its `@@` header.
    // When both reach zero the hunk is complete and further body lines belong
    // to the surrounding diff.
    old_remaining: u32,
    new_remaining: u32,
    // A pending git rename recorded before the `---`/`+++` lines land.
    pending_old_path: Option<String>,
    pending_new_path: Option<String>,
    pending_status: Option<FileStatus>,
}

impl Parser {
    fn feed(&mut self, line: usize, raw: &str) -> Result<(), ParseError> {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            self.start_file_from_git_header(rest);
        } else if let Some(rest) = raw.strip_prefix("--- ") {
            // In headerless formats (plain `diff -u`, svn, bzr) each file begins
            // with `---` rather than `diff --git`, so a `---` arriving once the
            // current file already has hunk content starts the next file.
            if self.hunk.is_some() || self.current_has_hunks() {
                self.finish_file();
            }
            self.ensure_file();
            let path = header_path(rest);
            if let Some(file) = self.current.as_mut() {
                match path {
                    Some(path) => file.old_path = path,
                    None => file.status = FileStatus::Added,
                }
            }
        } else if let Some(rest) = raw.strip_prefix("+++ ") {
            self.ensure_file();
            let path = header_path(rest);
            if let Some(file) = self.current.as_mut() {
                match path {
                    Some(path) => file.new_path = path,
                    None => file.status = FileStatus::Deleted,
                }
            }
        } else if raw.starts_with("@@") {
            self.start_hunk(line, raw)?;
        } else if self.hunk.is_some() {
            self.feed_hunk_line(line, raw)?;
        } else {
            self.feed_extended_header(raw);
        }
        Ok(())
    }

    fn start_file_from_git_header(&mut self, rest: &str) {
        self.finish_file();
        // `a/path b/path`; when the path has no spaces this split is exact.
        let (old, new) = split_git_paths(rest);
        self.current = Some(FileDiff {
            old_path: old,
            new_path: new,
            status: FileStatus::Modified,
            hunks: Vec::new(),
        });
    }

    fn feed_extended_header(&mut self, raw: &str) {
        if raw.starts_with("new file mode") {
            self.pending_status = Some(FileStatus::Added);
            if let Some(file) = self.current.as_mut() {
                file.status = FileStatus::Added;
            }
        } else if raw.starts_with("deleted file mode") {
            self.pending_status = Some(FileStatus::Deleted);
            if let Some(file) = self.current.as_mut() {
                file.status = FileStatus::Deleted;
            }
        } else if let Some(path) = raw.strip_prefix("rename from ") {
            self.pending_old_path = Some(path.trim().to_string());
            self.mark_renamed();
        } else if let Some(path) = raw.strip_prefix("rename to ") {
            self.pending_new_path = Some(path.trim().to_string());
            self.mark_renamed();
        }
    }

    fn mark_renamed(&mut self) {
        self.pending_status = Some(FileStatus::Renamed);
        if let Some(file) = self.current.as_mut() {
            file.status = FileStatus::Renamed;
            if let Some(old) = self.pending_old_path.clone() {
                file.old_path = old;
            }
            if let Some(new) = self.pending_new_path.clone() {
                file.new_path = new;
            }
        }
    }

    fn ensure_file(&mut self) {
        if self.current.is_none() {
            self.current = Some(FileDiff {
                old_path: String::new(),
                new_path: String::new(),
                status: self.pending_status.unwrap_or(FileStatus::Modified),
                hunks: Vec::new(),
            });
        }
    }

    fn start_hunk(&mut self, line: usize, raw: &str) -> Result<(), ParseError> {
        self.finish_hunk();
        self.ensure_file();
        let header = parse_hunk_header(raw).ok_or_else(|| ParseError::BadHunkHeader {
            line,
            content: raw.to_string(),
        })?;
        self.old_lineno = header.old_start;
        self.new_lineno = header.new_start;
        self.old_remaining = header.old_len;
        self.new_remaining = header.new_len;
        self.hunk = Some(Hunk {
            old_start: header.old_start,
            old_len: header.old_len,
            new_start: header.new_start,
            new_len: header.new_len,
            section: header.section,
            lines: Vec::new(),
        });
        Ok(())
    }

    fn feed_hunk_line(&mut self, line: usize, raw: &str) -> Result<(), ParseError> {
        // A "\ No newline at end of file" marker annotates the prior line; the
        // model records text without newlines, so it is simply ignored.
        if raw.starts_with('\\') {
            return Ok(());
        }
        // Once the hunk has consumed its declared line counts, a further body
        // line is not part of it: close the hunk and reinterpret the line as
        // surrounding content (a following file marker or trailing prose).
        if self.old_remaining == 0 && self.new_remaining == 0 {
            self.finish_hunk();
            self.feed_extended_header(raw);
            return Ok(());
        }
        let hunk = self.hunk.as_mut().ok_or_else(|| ParseError::OrphanLine {
            line,
            content: raw.to_string(),
        })?;
        let (kind, text) = match raw.chars().next() {
            Some('+') => (LineKind::Added, &raw[1..]),
            Some('-') => (LineKind::Removed, &raw[1..]),
            Some(' ') => (LineKind::Context, &raw[1..]),
            // A fully empty line in a diff body denotes an empty context line.
            None => (LineKind::Context, ""),
            _ => {
                return Err(ParseError::OrphanLine {
                    line,
                    content: raw.to_string(),
                });
            }
        };
        let (old_lineno, new_lineno) = match kind {
            LineKind::Context => {
                let pair = (LineNo::new(self.old_lineno), LineNo::new(self.new_lineno));
                self.old_lineno += 1;
                self.new_lineno += 1;
                self.old_remaining = self.old_remaining.saturating_sub(1);
                self.new_remaining = self.new_remaining.saturating_sub(1);
                pair
            }
            LineKind::Removed => {
                let pair = (LineNo::new(self.old_lineno), None);
                self.old_lineno += 1;
                self.old_remaining = self.old_remaining.saturating_sub(1);
                pair
            }
            LineKind::Added => {
                let pair = (None, LineNo::new(self.new_lineno));
                self.new_lineno += 1;
                self.new_remaining = self.new_remaining.saturating_sub(1);
                pair
            }
        };
        hunk.lines.push(DiffLine {
            kind,
            text: text.to_string(),
            old_lineno,
            new_lineno,
        });
        Ok(())
    }

    fn current_has_hunks(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|file| !file.hunks.is_empty())
    }

    fn finish_hunk(&mut self) {
        if let Some(hunk) = self.hunk.take()
            && let Some(file) = self.current.as_mut()
        {
            file.hunks.push(hunk);
        }
    }

    fn finish_file(&mut self) {
        self.finish_hunk();
        if let Some(mut file) = self.current.take() {
            infer_status_from_hunks(&mut file);
            self.files.push(file);
        }
        self.pending_old_path = None;
        self.pending_new_path = None;
        self.pending_status = None;
    }

    fn finish(&mut self) {
        self.finish_file();
    }
}

/// Split the `a/old b/new` tail of a `diff --git` header. Paths without spaces
/// split cleanly at the midpoint separator; the `---`/`+++` lines correct the
/// rare space-containing case.
fn split_git_paths(rest: &str) -> (String, String) {
    let parts: Vec<&str> = rest.split(' ').collect();
    if parts.len() == 2 {
        let old = header_path(parts[0]).unwrap_or_default();
        let new = header_path(parts[1]).unwrap_or_default();
        return (old, new);
    }
    // Ambiguous with spaces: fall back to the whole tail on both sides; the
    // `---`/`+++` headers overwrite these.
    let joined = header_path(rest).unwrap_or_default();
    (joined.clone(), joined)
}

/// Infer a file's status from its hunk coverage when the headers did not
/// already settle it. Formats without `diff --git` or `/dev/null` (svn, bzr)
/// leave a full add or delete looking like a plain edit, but the hunk ranges
/// still reveal it: an empty old side is an addition, an empty new side a
/// deletion.
fn infer_status_from_hunks(file: &mut FileDiff) {
    if file.status != FileStatus::Modified || file.hunks.is_empty() {
        return;
    }
    let old_lines: u32 = file.hunks.iter().map(|hunk| hunk.old_len).sum();
    let new_lines: u32 = file.hunks.iter().map(|hunk| hunk.new_len).sum();
    if old_lines == 0 && new_lines > 0 {
        file.status = FileStatus::Added;
    } else if new_lines == 0 && old_lines > 0 {
        file.status = FileStatus::Deleted;
    }
}

struct HunkHeader {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
    section: Option<String>,
}

/// Parse `@@ -old_start,old_len +new_start,new_len @@ section`. The lengths
/// default to 1 when omitted, matching unified diff conventions.
fn parse_hunk_header(raw: &str) -> Option<HunkHeader> {
    let after_marker = raw.strip_prefix("@@")?;
    let (ranges, section) = after_marker.split_once("@@")?;
    let mut parts = ranges.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    let section = {
        let trimmed = section.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    Some(HunkHeader {
        old_start,
        old_len,
        new_start,
        new_len,
        section,
    })
}

/// Parse a `start,len` or `start` range component.
fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}
