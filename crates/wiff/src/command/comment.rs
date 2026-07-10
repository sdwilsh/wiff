//! `wiff comment`: author comments against a session.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use ulid::Ulid;
use wiff_config::Config;
use wiff_core::record::{Author, AuthorKind, CommentTarget};
use wiff_core::review::ReviewState;
use wiff_core::{DraftComment, SessionLog, delete_comment, set_resolved};
use wiff_diff::{LineNo, Side};

use super::{read_piped_stdin, resolve_session};
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
            CommentCommand::List(args) => args.run(),
            CommentCommand::Resolve(args) => args.run(),
            CommentCommand::Rm(args) => args.run(),
        }
    }
}

/// The `wiff comment` subcommands.
#[derive(Debug, Subcommand)]
enum CommentCommand {
    /// Append a comment to a session.
    Add(CommentAddArgs),
    /// List a session's comments with their ids.
    List(CommentListArgs),
    /// Mark a comment resolved, or reopen it.
    Resolve(CommentResolveArgs),
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
    /// The comment body. When omitted, it is read from stdin.
    #[arg(long)]
    body: Option<String>,
    /// The author's display name.
    #[arg(long)]
    author: Option<String>,
    /// Attribute the comment to an agent rather than a human.
    #[arg(long)]
    agent: bool,
    /// Comment on a specific session by ULID instead of the active one.
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
        let target = self.target()?;
        let body = comment_body(self.body.clone()).await?;
        let author = resolve_author(self.agent, self.author.clone())?;
        let mut log = SessionLog::open(&path)?;
        let added = DraftComment {
            author,
            target,
            body,
        }
        .append(&mut log)?;
        println!("added comment {} (seq {})", added.id, added.seq);
        Ok(())
    }

    /// Resolve the comment's target from the target-selecting flags.
    fn target(&self) -> anyhow::Result<CommentTarget> {
        if self.review {
            return Ok(CommentTarget::Review);
        }
        let Some(file) = self.file.clone() else {
            bail!("specify a target with --file, --line, or --review");
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

/// Arguments for `wiff comment list`.
#[derive(Debug, Args)]
struct CommentListArgs {
    /// List a specific session by ULID instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl CommentListArgs {
    /// Print the session's live comments, id first.
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
    /// The id of the comment to resolve.
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
    /// Resolve a comment in a specific session by ULID instead of the active one.
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
        let id = parse_id(&self.id)?;
        let author = resolve_author(self.agent, self.author.clone())?;
        let mut log = SessionLog::open(&path)?;
        let comment = set_resolved(&mut log, id, !self.reopen, author)?;
        let verb = if self.reopen { "reopened" } else { "resolved" };
        println!("{verb} comment {}", comment.id);
        Ok(())
    }
}

/// Arguments for `wiff comment rm`.
#[derive(Debug, Args)]
struct CommentRmArgs {
    /// The id of the comment to withdraw.
    id: String,
    /// The author's display name.
    #[arg(long)]
    author: Option<String>,
    /// Attribute the withdrawal to an agent rather than a human.
    #[arg(long)]
    agent: bool,
    /// Withdraw a comment in a specific session by ULID instead of the active one.
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
        let id = parse_id(&self.id)?;
        let author = resolve_author(self.agent, self.author.clone())?;
        let mut log = SessionLog::open(&path)?;
        let comment = delete_comment(&mut log, id, author)?;
        println!("withdrew comment {}", comment.id);
        Ok(())
    }
}

/// Parse a comment id from its ULID text.
fn parse_id(id: &str) -> anyhow::Result<Ulid> {
    Ulid::from_string(id).with_context(|| format!("{id} is not a valid comment id"))
}

/// The author an action is attributed to: the default name for the kind acted
/// as -- an agent annotates as "assistant", a human as $USER -- honoring any
/// configured name, which an explicit `--author` overrides in turn.
fn resolve_author(agent: bool, name: Option<String>) -> anyhow::Result<Author> {
    let kind = if agent {
        AuthorKind::Agent
    } else {
        AuthorKind::Human
    };
    let mut author = Config::load()?.author.resolve(kind);
    if let Some(name) = name {
        author.name = name;
    }
    Ok(author)
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
