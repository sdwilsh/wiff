//! Tab expansion for diff text.
//!
//! A raw tab advances by a terminal-dependent amount, breaking the alignment of
//! the gutter and the code beside it in a fixed-column layout. Expanding tabs to
//! spaces at parse time lets all downstream code work with a single column
//! model.

use crate::model::Diff;

/// The default number of columns a tab expands to.
pub const DEFAULT_TAB_WIDTH: usize = 4;

/// Expand tab characters in `text` to spaces, each tab advancing to the next
/// multiple of `width` columns, the way an editor set to that tab width shows
/// it. Columns are counted in characters, and an embedded newline restarts the
/// count.
pub fn expand_tabs(text: &str, width: usize) -> String {
    let tab_count = text.matches('\t').count();
    if width == 0 || tab_count == 0 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + tab_count * width);
    let mut col = 0;
    for ch in text.chars() {
        match ch {
            '\t' => {
                let advance = width - (col % width);
                for _ in 0..advance {
                    out.push(' ');
                }
                col += advance;
            }
            '\n' => {
                out.push(ch);
                col = 0;
            }
            _ => {
                out.push(ch);
                col += 1;
            }
        }
    }
    out
}

impl Diff {
    /// Expand tab characters in every line's text to spaces at `tab_width`
    /// column tab stops. Line numbering and hunk structure are untouched; only
    /// the text of each line changes.
    pub fn expand_tabs(&mut self, tab_width: usize) {
        for file in &mut self.files {
            for hunk in &mut file.hunks {
                for line in &mut hunk.lines {
                    if line.text.contains('\t') {
                        line.text = expand_tabs(&line.text, tab_width);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::expand_tabs;
    use crate::model::{Diff, DiffLine, FileDiff, FileStatus, Hunk, LineKind};

    #[test]
    fn expanding_a_diff_rewrites_every_line_text() {
        let mut diff = Diff {
            files: vec![FileDiff {
                old_path: "a.rs".to_string(),
                new_path: "a.rs".to_string(),
                status: FileStatus::Modified,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_len: 1,
                    new_start: 1,
                    new_len: 1,
                    section: None,
                    lines: vec![
                        DiffLine {
                            kind: LineKind::Context,
                            text: "\tkept".to_string(),
                            old_lineno: None,
                            new_lineno: None,
                        },
                        DiffLine {
                            kind: LineKind::Added,
                            text: "if x\t{".to_string(),
                            old_lineno: None,
                            new_lineno: None,
                        },
                    ],
                }],
            }],
        };
        diff.expand_tabs(4);
        let texts: Vec<&str> = diff.files[0].hunks[0]
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        wince::assert_eq!(texts, vec!["    kept", "if x    {"]);
    }

    #[test]
    fn a_leading_tab_fills_to_the_first_stop() {
        wince::assert_eq!(expand_tabs("\tx", 4), "    x".to_string());
    }

    #[test]
    fn a_tab_advances_to_the_next_stop_not_a_fixed_width() {
        wince::assert_eq!(expand_tabs("ab\tc", 4), "ab  c".to_string());
    }

    #[test]
    fn a_tab_at_a_stop_boundary_advances_a_full_width() {
        wince::assert_eq!(expand_tabs("abcd\te", 4), "abcd    e".to_string());
    }

    #[test]
    fn successive_tabs_each_advance_to_a_stop() {
        wince::assert_eq!(expand_tabs("\t\tx", 4), "        x".to_string());
    }

    #[test]
    fn text_without_tabs_is_returned_unchanged() {
        wince::assert_eq!(expand_tabs("plain text", 4), "plain text".to_string());
    }

    #[test]
    fn a_zero_width_leaves_tabs_in_place() {
        wince::assert_eq!(expand_tabs("\tx", 0), "\tx".to_string());
    }
}
