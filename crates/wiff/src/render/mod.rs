//! Rendering a folded [`ReviewState`] for consumption.
//!
//! `wiff render` emits the current review state either as markdown for a human
//! or an agent prompt, or as JSON for programmatic use. Both are derived from
//! the same folded state via [`render`]; the per-format detail lives in the
//! `markdown` and `json` submodules.

mod json;
mod markdown;

use clap::ValueEnum;
use wiff_core::record::FileSummary;
use wiff_core::review::{CommentState, ReviewState};

/// The output format for `wiff render`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Human- and agent-readable markdown.
    Markdown,
    /// The folded current state as JSON.
    Json,
}

/// Render `state` in the requested `format`.
pub fn render(state: &ReviewState, format: Format) -> anyhow::Result<String> {
    match format {
        Format::Markdown => Ok(markdown::render(state)),
        Format::Json => json::render(state),
    }
}

/// The current (non-withdrawn) comments in creation order.
fn live_comments(state: &ReviewState) -> Vec<&CommentState> {
    state.comments.iter().filter(|c| !c.deleted).collect()
}

/// The files of the latest diff version, or an empty slice when none.
fn latest_files(state: &ReviewState) -> &[FileSummary] {
    state
        .latest_version()
        .map(|version| version.files.as_slice())
        .unwrap_or(&[])
}

#[cfg(test)]
mod fixture {
    use ulid::Ulid;
    use wiff_core::SidebandHash;
    use wiff_core::record::{
        Anchor, Author, AuthorKind, CommentTarget, Confidence, DiffVersionRecord, FORMAT_VERSION,
        FileSummary, SessionHeader, SourceKind,
    };
    use wiff_core::review::{CommentState, ReviewState};
    use wiff_diff::{FileStatus, LineNo, Side};

    fn ulid(text: &str) -> Ulid {
        Ulid::from_string(text).unwrap()
    }

    fn author(name: &str, kind: AuthorKind) -> Author {
        Author {
            name: name.to_string(),
            kind,
        }
    }

    fn comment(
        id: &str,
        author: Author,
        target: CommentTarget,
        body: &str,
        seq: u64,
    ) -> CommentState {
        CommentState {
            id: ulid(id),
            author,
            target,
            version: 0,
            anchor: None,
            body: body.to_string(),
            resolved: false,
            deleted: false,
            confidence: None,
            created_seq: seq,
            updated_seq: seq,
        }
    }

    fn lines(file: &str, start: u32, end: u32) -> CommentTarget {
        CommentTarget::Lines {
            file: file.to_string(),
            side: Side::After,
            start_line: LineNo::new(start).unwrap(),
            end_line: LineNo::new(end).unwrap(),
        }
    }

    /// A review state exercising every comment target, an anchored range, a
    /// re-anchored comment, and a withdrawn comment.
    pub(super) fn state() -> ReviewState {
        let mut line = comment(
            "00000000000000000000000001",
            author("wez", AuthorKind::Human),
            lines("main.rs", 2, 2),
            "why 3?",
            2,
        );
        line.anchor = Some(Anchor {
            snippet: vec!["let b = 3;".to_string()],
            context_before: vec!["let a = 1;".to_string()],
            context_after: vec!["let c = 4;".to_string()],
        });
        let whole = comment(
            "00000000000000000000000002",
            author("assistant", AuthorKind::Agent),
            CommentTarget::File {
                file: "main.rs".to_string(),
            },
            "needs tests",
            3,
        );
        let review = comment(
            "00000000000000000000000003",
            author("wez", AuthorKind::Human),
            CommentTarget::Review,
            "overall solid",
            4,
        );
        let mut shifted = comment(
            "00000000000000000000000004",
            author("dev", AuthorKind::Human),
            lines("other.rs", 5, 6),
            "moved code",
            5,
        );
        shifted.confidence = Some(Confidence::Approximate);
        shifted.updated_seq = 6;
        let mut gone = comment(
            "00000000000000000000000005",
            author("wez", AuthorKind::Human),
            lines("main.rs", 9, 9),
            "never mind",
            7,
        );
        gone.deleted = true;
        gone.updated_seq = 8;

        ReviewState {
            session: SessionHeader {
                ulid: ulid("00000000000000000000000000"),
                version: FORMAT_VERSION,
                project: "demo".to_string(),
                repo_root: Some("/repos/demo".to_string()),
                cwd: "/repos/demo".to_string(),
                source: SourceKind::GitWorktree,
            },
            versions: vec![DiffVersionRecord {
                number: 0,
                diff_hash: SidebandHash::of(b"main.rs"),
                files: vec![FileSummary {
                    old_path: "main.rs".to_string(),
                    new_path: "main.rs".to_string(),
                    status: FileStatus::Modified,
                    hunk_count: 1,
                }],
            }],
            comments: vec![line, whole, review, shifted, gone],
        }
    }
}
