//! `wiff comment`: author comments against a session.

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand};
use ulid::Ulid;
use wiff_core::record::{CommentRef, CommentTarget, Disposition};
use wiff_core::review::ReviewState;
use wiff_core::{
    DraftComment, LockWait, SessionLog, delete_comment, edit_comment, set_disposition, set_resolved,
};
use wiff_diff::{LineNo, Side};

use super::{read_piped_stdin, resolve_author, resolve_session};
use crate::render::render_list;

/// Arguments for `wiff comment`.
#[derive(Debug, Args)]
pub struct CommentArgs {
    #[command(subcommand)]
    command: CommentCommand,
}

impl CommentArgs {
    /// Dispatch the selected `wiff comment` subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            CommentCommand::Add(args) => args.run().await,
            CommentCommand::Edit(args) => args.run().await,
            CommentCommand::List(args) => args.run(),
            CommentCommand::Resolve(args) => args.run(),
            CommentCommand::Verdict(args) => args.run(),
            CommentCommand::Rm(args) => args.run(),
        }
    }
}

/// The `wiff comment` subcommands.
#[derive(Debug, Subcommand)]
enum CommentCommand {
    /// Append a comment to a session.
    Add(CommentAddArgs),
    /// Rewrite a comment's body.
    Edit(CommentEditArgs),
    /// List a session's comments with their ids.
    List(CommentListArgs),
    /// Mark a comment resolved, or reopen it.
    Resolve(CommentResolveArgs),
    /// Set or clear a comment's verdict.
    Verdict(CommentVerdictArgs),
    /// Withdraw a comment.
    Rm(CommentRmArgs),
}

/// Arguments for `wiff comment add`.
#[derive(Debug, Args)]
struct CommentAddArgs {
    /// The file to comment on. Without `--line` this is a whole-file comment.
    #[arg(long)]
    file: Option<String>,
    /// A line or inclusive line range within `--file`, as `N` or `N-M`.
    #[arg(long, requires = "file")]
    line: Option<String>,
    /// Which side of the diff `--line` refers to.
    #[arg(long, value_enum, default_value_t = SideArg::After, requires = "line")]
    side: SideArg,
    /// Comment on the review overall rather than a file or line.
    #[arg(long, conflicts_with_all = ["file", "line"])]
    review: bool,
    /// Reply to an existing comment, named by its number or ULID, forming a
    /// thread. The reply takes its position from the comment it answers rather
    /// than a file or line.
    #[arg(long, conflicts_with_all = ["file", "line", "review"])]
    reply_to: Option<String>,
    /// The comment body. When omitted, it is read from stdin.
    #[arg(long)]
    body: Option<String>,
    /// This comment's verdict: sign off, ask for changes, or `none` to leave it
    /// a neutral remark (the default).
    #[arg(long, value_enum)]
    verdict: Option<VerdictArg>,
    /// The author's display name.
    #[arg(long)]
    author: Option<String>,
    /// Attribute the comment to an agent rather than a human.
    #[arg(long)]
    agent: bool,
    /// Comment on a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl CommentAddArgs {
    /// Append a comment to a session and report its id.
    async fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let author = resolve_author(self.agent, self.author.clone())?;
        // On an explore review, commenting on a file not yet under review adds
        // it first, so `--file` doubles as the way to bring a file in. This is a
        // no-op on any other kind of session, and returns the file's stored
        // spelling for the target to anchor against.
        let file = match &self.file {
            Some(file) => Some(super::explore::ensure_file_present(
                &path,
                file,
                author.clone(),
            )?),
            None => None,
        };
        let target = self.target(&path, file)?;
        let body = comment_body(self.body.clone()).await?;
        let mut log = SessionLog::open(&path)?;
        let added = DraftComment {
            author,
            target,
            body,
            disposition: self.verdict.and_then(VerdictArg::into_disposition),
        }
        .append(&mut log, LockWait::Block)?;
        println!("added comment {} (seq {})", added.id, added.seq);
        Ok(())
    }

    /// Resolve the comment's target from the target-selecting flags. A
    /// `--reply-to` reference is resolved against `path`'s folded state so it
    /// accepts a comment number as readily as a ULID. `file` is the resolved
    /// `--file` value, already normalized to the stored spelling for an explore
    /// session.
    fn target(&self, path: &Path, file: Option<String>) -> anyhow::Result<CommentTarget> {
        if self.review {
            return Ok(CommentTarget::Review);
        }
        if let Some(reference) = &self.reply_to {
            return Ok(CommentTarget::Comment {
                id: resolve_comment_ref(path, reference)?,
            });
        }
        let Some(file) = file else {
            bail!("specify a target with --file, --line, --review, or --reply-to");
        };
        match &self.line {
            Some(spec) => {
                let (start_line, end_line) = parse_line_range(spec)?;
                Ok(CommentTarget::Lines {
                    file,
                    side: self.side.into(),
                    start_line,
                    end_line,
                })
            }
            None => Ok(CommentTarget::File { file }),
        }
    }
}

/// Arguments for `wiff comment edit`.
#[derive(Debug, Args)]
struct CommentEditArgs {
    /// The comment to edit, named by its number or ULID. A human may edit any
    /// comment; an agent may edit only its own.
    id: String,
    /// The new body. When omitted, it is read from stdin.
    #[arg(long)]
    body: Option<String>,
    /// The author's display name.
    #[arg(long)]
    author: Option<String>,
    /// Attribute the edit to an agent rather than a human.
    #[arg(long)]
    agent: bool,
    /// Edit a comment in a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl CommentEditArgs {
    /// Rewrite a comment's body and report the outcome.
    async fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let id = resolve_comment_ref(&path, &self.id)?;
        let author = resolve_author(self.agent, self.author.clone())?;
        let body = comment_body(self.body.clone()).await?;
        let mut log = SessionLog::open(&path)?;
        let comment = edit_comment(&mut log, id, body, author, LockWait::Block)?;
        // Echo the ULID, not the number: it shows the cross-session identity of
        // the comment the user named.
        println!("edited comment {}", comment.id);
        Ok(())
    }
}

/// Arguments for `wiff comment list`.
#[derive(Debug, Args)]
struct CommentListArgs {
    /// List a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl CommentListArgs {
    /// Print the session's live comments, number first.
    fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let state = ReviewState::load(&path)?;
        print!("{}", render_list(&state));
        Ok(())
    }
}

/// Arguments for `wiff comment resolve`.
#[derive(Debug, Args)]
struct CommentResolveArgs {
    /// The comment to resolve, named by its number or ULID.
    id: String,
    /// Reopen the comment instead of resolving it.
    #[arg(long)]
    reopen: bool,
    /// The author's display name.
    #[arg(long)]
    author: Option<String>,
    /// Attribute the change to an agent rather than a human.
    #[arg(long)]
    agent: bool,
    /// Resolve a comment in a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl CommentResolveArgs {
    /// Toggle a comment's resolved state and report the outcome.
    fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let id = resolve_comment_ref(&path, &self.id)?;
        let author = resolve_author(self.agent, self.author.clone())?;
        let mut log = SessionLog::open(&path)?;
        let comment = set_resolved(&mut log, id, !self.reopen, author, LockWait::Block)?;
        let verb = if self.reopen { "reopened" } else { "resolved" };
        // The user named the comment, so echo the ULID rather than the number:
        // it shows the underlying cross-session identity of what they acted on.
        println!("{verb} comment {}", comment.id);
        Ok(())
    }
}

/// Arguments for `wiff comment verdict`.
#[derive(Debug, Args)]
struct CommentVerdictArgs {
    /// The comment to set a verdict on, named by its number or ULID. Only its
    /// author may.
    id: String,
    /// The verdict: `approve`, `request_changes`, or `none` to return to
    /// neutral.
    verdict: VerdictArg,
    /// The author's display name.
    #[arg(long)]
    author: Option<String>,
    /// Attribute the change to an agent rather than a human.
    #[arg(long)]
    agent: bool,
    /// Act on a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl CommentVerdictArgs {
    /// Set or clear a comment's verdict and report the outcome.
    fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let id = resolve_comment_ref(&path, &self.id)?;
        let author = resolve_author(self.agent, self.author.clone())?;
        let disposition = self.verdict.into_disposition();
        let mut log = SessionLog::open(&path)?;
        let comment = set_disposition(&mut log, id, disposition, author, LockWait::Block)?;
        // Echo the ULID, not the number: it shows the cross-session identity of
        // the comment the user named.
        println!("{}", verdict_outcome(comment.id, disposition));
        Ok(())
    }
}

/// The line reported after setting or clearing comment `id`'s verdict.
fn verdict_outcome(id: Ulid, disposition: Option<Disposition>) -> String {
    match disposition {
        Some(disposition) => format!("set comment {id} to {}", disposition.as_str()),
        None => format!("cleared the verdict on comment {id}"),
    }
}

/// Arguments for `wiff comment rm`.
#[derive(Debug, Args)]
struct CommentRmArgs {
    /// The comment to withdraw, named by its number or ULID. A human may
    /// withdraw any comment; an agent may withdraw only its own.
    id: String,
    /// The author's display name.
    #[arg(long)]
    author: Option<String>,
    /// Attribute the withdrawal to an agent rather than a human.
    #[arg(long)]
    agent: bool,
    /// Withdraw a comment in a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl CommentRmArgs {
    /// Withdraw a comment and report the outcome.
    fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let id = resolve_comment_ref(&path, &self.id)?;
        let author = resolve_author(self.agent, self.author.clone())?;
        let mut log = SessionLog::open(&path)?;
        let comment = delete_comment(&mut log, id, author, LockWait::Block)?;
        // Echo the ULID, not the number: it shows the cross-session identity of
        // the comment the user named.
        println!("withdrew comment {}", comment.id);
        Ok(())
    }
}

/// A verdict word accepted on the command line: sign off, ask for changes, or
/// `none` for a neutral remark. `add` and `verdict` share it; on `add`, `none`
/// (or omitting the flag) leaves the comment without a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum VerdictArg {
    /// Sign off on the change.
    Approve,
    /// Ask for changes before the change is accepted.
    #[value(name = "request_changes")]
    RequestChanges,
    /// Leave the comment a neutral remark, or return it to one.
    None,
}

impl VerdictArg {
    /// The disposition this word sets, or `None` for a neutral remark.
    fn into_disposition(self) -> Option<Disposition> {
        match self {
            VerdictArg::Approve => Some(Disposition::Approve),
            VerdictArg::RequestChanges => Some(Disposition::RequestChanges),
            VerdictArg::None => Option::None,
        }
    }
}

/// Returns the stable id of the comment named by `reference`, a full ULID or a
/// review-scoped number.
///
/// A ULID passes straight through without a read; the mutation it feeds
/// validates the comment's existence under its own lock. A number is looked up
/// in a folded snapshot read here without the lock, which is sound because the
/// log never renumbers (append-only, withdrawals are tombstones), so a number
/// maps to the same comment across the gap to the locked mutation. The one
/// visible effect of the unlocked read is that a number a concurrent writer only
/// just appended can be refused as unknown until the next read; it never
/// resolves to the wrong comment.
fn resolve_comment_ref(path: &Path, reference: &str) -> anyhow::Result<Ulid> {
    let reference: CommentRef = reference.parse().map_err(|message| anyhow!("{message}"))?;
    match reference {
        CommentRef::Ulid(id) => Ok(id),
        reference => Ok(ReviewState::load(path)?.resolve_ref(reference)?),
    }
}

/// Which side of the diff a line-range comment refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SideArg {
    /// The pre-change content (context and removed lines).
    Before,
    /// The post-change content (context and added lines).
    After,
}

impl From<SideArg> for Side {
    fn from(side: SideArg) -> Self {
        match side {
            SideArg::Before => Side::Before,
            SideArg::After => Side::After,
        }
    }
}

/// Parse a `--line` value: a single line `N` or an inclusive range `N-M`.
fn parse_line_range(spec: &str) -> anyhow::Result<(LineNo, LineNo)> {
    let parse_one = |text: &str| -> anyhow::Result<LineNo> {
        let n: u32 = text
            .trim()
            .parse()
            .with_context(|| format!("{spec} is not a valid line or line range"))?;
        LineNo::new(n).with_context(|| format!("line numbers start at 1, but {spec} includes 0"))
    };
    let (start, end) = match spec.split_once('-') {
        Some((start, end)) => (parse_one(start)?, parse_one(end)?),
        None => {
            let only = parse_one(spec)?;
            (only, only)
        }
    };
    if end < start {
        bail!("line range {spec} ends before it starts");
    }
    Ok((start, end))
}

/// The comment body: the `--body` value, else read from stdin.
async fn comment_body(body: Option<String>) -> anyhow::Result<String> {
    if let Some(body) = body {
        return Ok(body);
    }
    let Some(text) = read_piped_stdin().await? else {
        bail!("provide the comment with --body or pipe it on stdin");
    };
    Ok(text.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ulid::Ulid;
    use wiff_core::record::{Author, AuthorKind, CommentTarget, Disposition, SourceKind};
    use wiff_core::{CapturedDiff, DraftComment, LockWait, ProjectIdentity, create_session};

    use super::{VerdictArg, resolve_comment_ref, verdict_outcome};

    const DIFF: &str = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -1 +1 @@
-old
+new
";

    /// A review with two committed review comments, so their numbers are `#1`
    /// and `#2` in the order they were added.
    fn two_comment_session(base: &Path) -> (std::path::PathBuf, Ulid, Ulid) {
        let captured = CapturedDiff {
            text: DIFF.to_string(),
            source: SourceKind::Stdin,
            base_revision: None,
            base_tip_relative: false,
            head_revision: None,
        };
        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: None,
            scm: None,
        };
        let mut log = create_session(base, &identity, Path::new("/work"), &captured, None).unwrap();
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        let add = |log: &mut _, body: &str| {
            DraftComment {
                author: author.clone(),
                target: CommentTarget::Review,
                body: body.to_string(),
                disposition: None,
            }
            .append(log, LockWait::Block)
            .unwrap()
            .id
        };
        let first = add(&mut log, "first");
        let second = add(&mut log, "second");
        (log.path().to_path_buf(), first, second)
    }

    #[test]
    fn a_comment_reference_resolves_a_number_a_hash_number_and_a_ulid() {
        let base = tempfile::tempdir().unwrap();
        let (path, first, second) = two_comment_session(base.path());
        let resolved: Vec<Option<Ulid>> = ["1", "#2", &second.to_string(), "9", "nope"]
            .into_iter()
            .map(|reference| resolve_comment_ref(&path, reference).ok())
            .collect();
        wince::assert_eq!(
            resolved,
            vec![Some(first), Some(second), Some(second), None, None]
        );
    }

    #[test]
    fn an_unknown_number_reports_the_reference_it_could_not_resolve() {
        let base = tempfile::tempdir().unwrap();
        let (path, _first, _second) = two_comment_session(base.path());
        let message = resolve_comment_ref(&path, "9").unwrap_err().to_string();
        wince::assert_eq!(message, "no comment #9 in this session".to_string());
    }

    #[test]
    fn each_verdict_word_maps_to_its_disposition_or_clears_it() {
        let mapped: Vec<Option<Disposition>> = [
            VerdictArg::Approve,
            VerdictArg::RequestChanges,
            VerdictArg::None,
        ]
        .into_iter()
        .map(VerdictArg::into_disposition)
        .collect();
        wince::assert_eq!(
            mapped,
            vec![
                Some(Disposition::Approve),
                Some(Disposition::RequestChanges),
                None,
            ]
        );
    }

    #[test]
    fn the_outcome_line_reports_the_verdict_set_or_that_it_was_cleared() {
        let id = Ulid::from_string("00000000000000000000000001").unwrap();
        let lines: Vec<String> = [
            Some(Disposition::Approve),
            Some(Disposition::RequestChanges),
            None,
        ]
        .into_iter()
        .map(|disposition| verdict_outcome(id, disposition))
        .collect();
        wince::assert_eq!(
            lines,
            vec![
                "set comment 00000000000000000000000001 to approve".to_string(),
                "set comment 00000000000000000000000001 to request_changes".to_string(),
                "cleared the verdict on comment 00000000000000000000000001".to_string(),
            ]
        );
    }
}
