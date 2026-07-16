//! The `wiff` subcommands: one module per command, each owning its arguments
//! and its `run` entry point. This module wires them into the top-level
//! [`Command`] enum and holds the few helpers shared across commands.

mod comment;
mod new;
mod refresh;
mod render;
mod resume;
mod session;
mod skill;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Subcommand;
use tokio::io::AsyncReadExt;
use ulid::Ulid;
use wiff_config::Config;
use wiff_core::record::{Author, AuthorKind, SessionHeader, SourceKind};
use wiff_core::session::{active_session, data_dir, session_file};
use wiff_core::{CapturedDiff, DiffSource, GitSource, ProjectIdentity, ScmType};

use self::comment::CommentArgs;
use self::new::NewArgs;
use self::refresh::RefreshArgs;
use self::render::RenderArgs;
use self::resume::ResumeArgs;
use self::session::SessionArgs;

/// The top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a review session from a source and open it.
    New(NewArgs),
    /// Resume an existing review session.
    Resume(ResumeArgs),
    /// Manage sessions.
    Session(SessionArgs),
    /// Capture a new diff version into a session and rebase comments.
    Refresh(RefreshArgs),
    /// Add or manage comments.
    Comment(CommentArgs),
    /// Render the review state for consumption.
    Render(RenderArgs),
    /// Write the agent skill file and print its path.
    SkillPath,
}

impl Command {
    /// Run the selected subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        match self {
            Command::New(args) => args.run().await,
            Command::Comment(args) => args.run().await,
            Command::Render(args) => args.run(),
            Command::Session(args) => args.run(),
            Command::Refresh(args) => args.run().await,
            Command::Resume(args) => args.run(),
            Command::SkillPath => skill::run(),
        }
    }
}

/// Resolve the session file to act on: the one named by `session`, else the
/// active session for the project derived from the cwd (or forced by `project`).
fn resolve_session(session: Option<&str>, project: Option<&str>) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let identity = ProjectIdentity::for_dir_or_forced(&cwd, project)?;
    let base = data_dir()?;
    match session {
        Some(session) => {
            let ulid = Ulid::from_string(session)
                .with_context(|| format!("{session} is not a valid session ULID"))?;
            Ok(session_file(&base, &identity.canonical, ulid))
        }
        None => Ok(active_session(&base, &identity.canonical)?),
    }
}

/// The author an action is attributed to: the default name for the kind acted
/// as -- an agent annotates as "assistant", a human as $USER -- honoring any
/// configured name, which an explicit `name` overrides in turn.
pub(crate) fn resolve_author(agent: bool, name: Option<String>) -> anyhow::Result<Author> {
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

/// Which slice of a repository to capture, independent of the source-control
/// system it lives in.
#[derive(Debug, Clone)]
pub(crate) enum DiffSelection {
    /// The uncommitted working tree.
    Worktree,
    /// The staged index against `HEAD`.
    Staged,
    /// The changes a single revision introduces.
    Rev(String),
}

/// Capture `selection` from the repository at `root` using its detected `scm`.
/// Errors when the repository is of a kind wiff cannot yet capture from, or when
/// there is nothing to review, so callers need not repeat those checks. This is
/// the single point that turns a selection into a running SCM command; adding a
/// new source-control system means adding its arm here.
pub(crate) async fn capture_scm_diff(
    scm: Option<ScmType>,
    root: PathBuf,
    selection: DiffSelection,
) -> anyhow::Result<CapturedDiff> {
    let source = match scm {
        Some(ScmType::Git) => match selection {
            DiffSelection::Worktree => GitSource::worktree(root),
            DiffSelection::Staged => GitSource::index(root),
            DiffSelection::Rev(rev) => GitSource::rev(root, rev),
        },
        Some(other) => bail!(
            "{} is a {other} repository, which wiff cannot capture from yet; pipe a unified diff on stdin instead",
            root.display()
        ),
        None => bail!(
            "{} is not a recognized repository; pipe a unified diff on stdin instead",
            root.display()
        ),
    };
    let captured = source.capture().await?;
    if captured.text.trim().is_empty() {
        bail!("no changes to review");
    }
    Ok(captured)
}

/// Recapture a session's diff from the source recorded in its `header`, or
/// `None` when that source is a one-shot diff (piped on stdin) that cannot be
/// regenerated. Both `wiff refresh` and the in-TUI refresh flow through here, so
/// the mapping from a recorded source back to a live capture lives in one place.
pub(crate) async fn recapture_diff(header: &SessionHeader) -> anyhow::Result<Option<String>> {
    let selection = match &header.source {
        SourceKind::GitWorktree => DiffSelection::Worktree,
        SourceKind::GitIndex => DiffSelection::Staged,
        SourceKind::GitRev { rev } => DiffSelection::Rev(rev.clone()),
        SourceKind::Stdin => return Ok(None),
    };
    let root = header
        .repo_root
        .clone()
        .context("the session records no repository root, so its diff cannot be recaptured")?;
    // Every regenerable source recorded today is a git one.
    let captured = capture_scm_diff(Some(ScmType::Git), root.into(), selection).await?;
    Ok(Some(captured.text))
}

/// Read content piped on stdin, returning `None` when stdin is a terminal or
/// carries nothing. Only content actually piped counts; a non-terminal but
/// empty stdin (a redirect, or a non-interactive harness) yields `None` so a
/// caller can fall back rather than treat it as an empty input.
async fn read_piped_stdin() -> anyhow::Result<Option<String>> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    // Warn an interactive user that we are about to block on their input, so a
    // command awaiting a terminal-less stdin does not look hung.
    if std::io::stderr().is_terminal() {
        eprintln!("Reading from stdin...");
    }
    let mut text = String::new();
    tokio::io::stdin()
        .read_to_string(&mut text)
        .await
        .context("could not read from stdin")?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}
