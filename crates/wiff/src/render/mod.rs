//! Rendering a folded [`ReviewState`] for consumption.
//!
//! `wiff render` emits the current review state either as markdown for a human
//! or an agent prompt, or as JSON for programmatic use. Both are derived from
//! the same folded state via [`render`]; the per-format detail lives in the
//! `markdown` and `json` submodules.

mod json;
mod list;
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

/// Render `state`'s comments as a compact, id-first list for `wiff comment
/// list`.
pub fn render_list(state: &ReviewState) -> String {
    list::render(state)
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
    use time::OffsetDateTime;
    use ulid::Ulid;
    use wiff_core::SidebandHash;
    use wiff_core::record::{
        Anchor, Author, AuthorKind, CommentTarget, Confidence, DiffVersionRecord, FORMAT_VERSION,
        FileSummary, Seq, SessionHeader, SourceKind, VersionNumber,
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
            author: author.clone(),
            target,
            version: VersionNumber(0),
            anchor: None,
            body: body.to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            updated_by: author,
            resolved: false,
            resolved_by: None,
            resolved_at: None,
            deleted: false,
            deleted_by: None,
            deleted_at: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            created_seq: Seq(seq),
            updated_seq: Seq(seq),
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
        let mut whole = comment(
            "00000000000000000000000002",
            author("assistant", AuthorKind::Agent),
            CommentTarget::File {
                file: "main.rs".to_string(),
            },
            "needs tests",
            3,
        );
        whole.resolved = true;
        whole.resolved_by = Some(author("wez", AuthorKind::Human));
        whole.resolved_at = Some(OffsetDateTime::UNIX_EPOCH);
        whole.updated_by = author("wez", AuthorKind::Human);
        whole.updated_seq = Seq(9);
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
        // An agent later edited this human's comment, so it is attributed to the
        // agent while its author stays the human. A reanchor alone would leave
        // `updated_by` as the author.
        shifted.updated_by = author("opus", AuthorKind::Agent);
        shifted.updated_seq = Seq(6);
        let mut gone = comment(
            "00000000000000000000000005",
            author("wez", AuthorKind::Human),
            lines("main.rs", 9, 9),
            "never mind",
            7,
        );
        gone.deleted = true;
        gone.deleted_by = Some(author("wez", AuthorKind::Human));
        gone.deleted_at = Some(OffsetDateTime::UNIX_EPOCH);
        gone.updated_seq = Seq(8);

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
                number: VersionNumber(0),
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
