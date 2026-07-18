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
use wiff_core::review::{CommentState, ReviewState, threads};

/// A thread reduced to what the read renderers show: its root and the replies
/// still live. A thread appears while its root is live or, once the root is
/// withdrawn, while any reply under it is still live; the withdrawn root then
/// shows as a tombstone so its replies stay reachable and no reply refers to a
/// root absent from the output. A thread all of whose comments are withdrawn
/// drops out entirely.
pub(super) struct VisibleThread<'a> {
    pub root: &'a CommentState,
    pub replies: Vec<&'a CommentState>,
}

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

/// Render `state`'s comments as a compact, number-first list for `wiff comment
/// list`.
pub fn render_list(state: &ReviewState) -> String {
    list::render(state)
}

/// The visible threads of `state`, in the order their roots were created.
fn visible_threads(state: &ReviewState) -> Vec<VisibleThread<'_>> {
    threads(&state.comments)
        .into_iter()
        .filter_map(|thread| {
            let replies: Vec<&CommentState> = thread
                .replies
                .into_iter()
                .filter(|reply| !reply.deleted)
                .collect();
            (!thread.root.deleted || !replies.is_empty()).then_some(VisibleThread {
                root: thread.root,
                replies,
            })
        })
        .collect()
}

/// The visible comments flattened in thread order: each root followed by its
/// live replies. A reply is never emitted without its root, which stays present
/// as a tombstone once withdrawn.
fn live_comments(state: &ReviewState) -> Vec<&CommentState> {
    visible_threads(state)
        .into_iter()
        .flat_map(|thread| std::iter::once(thread.root).chain(thread.replies))
        .collect()
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
    use wiff_core::record::{
        Anchor, Author, AuthorKind, CommentNumber, CommentTarget, Confidence, Description,
        DiffVersionRecord, Disposition, FORMAT_VERSION, FileSummary, ScmSource, Seq, SessionHeader,
        SourceKind, TipRule, VersionNumber,
    };
    use wiff_core::review::{ActorVerdict, CommentState, DescriptionState, ReviewState};
    use wiff_core::{BaseRuleset, ScmType, SidebandHash};
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
            disposition: None,
            confidence: None,
            origin: None,
            synced_marker: None,
            number: None,
            created_seq: Seq(seq),
            updated_seq: Seq(seq),
        }
    }

    /// Assign each comment its review-scoped number the way the fold does: by
    /// the log's append order, which the fold numbers from directly. Append
    /// order matches ascending `created_seq`, so sorting on that here rather
    /// than trusting the input order keeps the numbers faithful to a real fold
    /// even if the fixture's Vec is later reordered.
    fn number_in_order(mut comments: Vec<CommentState>) -> Vec<CommentState> {
        let mut by_creation: Vec<&mut CommentState> = comments.iter_mut().collect();
        by_creation.sort_by_key(|comment| comment.created_seq);
        for (index, comment) in by_creation.into_iter().enumerate() {
            comment.number = Some(CommentNumber(index as u32 + 1));
        }
        comments
    }

    fn lines(file: &str, start: u32, end: u32) -> CommentTarget {
        CommentTarget::Lines {
            file: file.to_string(),
            side: Side::After,
            start_line: LineNo::new(start).unwrap(),
            end_line: LineNo::new(end).unwrap(),
        }
    }

    /// A review state exercising a description, every comment target, an
    /// anchored range, a re-anchored comment, a withdrawn comment, and a
    /// withdrawn root kept visible by a live reply.
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
        let mut review = comment(
            "00000000000000000000000003",
            author("wez", AuthorKind::Human),
            CommentTarget::Review,
            "overall solid",
            4,
        );
        review.disposition = Some(Disposition::Approve);
        let reply = comment(
            "00000000000000000000000006",
            author("opus", AuthorKind::Agent),
            CommentTarget::Comment {
                id: ulid("00000000000000000000000001"),
            },
            "3 is the loop bound",
            10,
        );
        let mut shifted = comment(
            "00000000000000000000000004",
            author("dev", AuthorKind::Human),
            lines("other.rs", 5, 6),
            "moved code",
            5,
        );
        shifted.disposition = Some(Disposition::RequestChanges);
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
        // A live reply under the withdrawn root keeps the thread visible: the
        // root renders as a tombstone so the reply stays reachable.
        let reply_to_gone = comment(
            "00000000000000000000000007",
            author("dev", AuthorKind::Human),
            CommentTarget::Comment {
                id: ulid("00000000000000000000000005"),
            },
            "still relevant though",
            11,
        );

        ReviewState {
            session: SessionHeader {
                ulid: ulid("00000000000000000000000000"),
                version: FORMAT_VERSION,
                project: "demo".to_string(),
                repo_root: Some("/repos/demo".to_string()),
                cwd: "/repos/demo".to_string(),
                source: SourceKind::Scm(ScmSource {
                    scm: ScmType::Git,
                    base: BaseRuleset::new("ref(name(deadbeef))"),
                    tip: TipRule::Worktree,
                    branch_hint: None,
                }),
            },
            versions: vec![DiffVersionRecord {
                number: VersionNumber(0),
                diff_hash: SidebandHash::of(b"main.rs"),
                base_revision: None,
                base_tip_relative: false,
                head_revision: None,
                files: vec![FileSummary {
                    old_path: "main.rs".to_string(),
                    new_path: "main.rs".to_string(),
                    status: FileStatus::Modified,
                    hunk_count: 1,
                }],
            }],
            description: Some(DescriptionState {
                content: Description {
                    title: "Tidy the parser".to_string(),
                    body: "Split the lexer out and cover it with tests.".to_string(),
                },
                author: author("wez", AuthorKind::Human),
                updated_at: OffsetDateTime::UNIX_EPOCH,
                origin: None,
                synced_marker: None,
            }),
            comments: number_in_order(vec![
                line,
                whole,
                review,
                shifted,
                gone,
                reply,
                reply_to_gone,
            ]),
            verdicts: vec![
                ActorVerdict {
                    author: author("wez", AuthorKind::Human),
                    disposition: Disposition::Approve,
                },
                ActorVerdict {
                    author: author("dev", AuthorKind::Human),
                    disposition: Disposition::RequestChanges,
                },
            ],
        }
    }
}
