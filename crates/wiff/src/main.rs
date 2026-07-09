//! The `wiff` command-line entry point.

use std::io::IsTerminal;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use tokio::io::AsyncReadExt;
use wiff_core::record::SourceKind;
use wiff_core::session::data_dir;
use wiff_core::{
    CapturedDiff, DiffSource, GitSource, ProjectIdentity, ScmType, SessionLog, create_session,
};

/// wiff: sniff out the wiff in your diff, from the comfort of your terminal.
#[derive(Debug, Parser)]
#[command(name = "wiff", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The top-level subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Create a review session from a source and open it.
    New(NewArgs),
    /// Resume an existing review session.
    Resume,
    /// Manage sessions.
    Session,
    /// Capture a new diff version into a session and rebase comments.
    Refresh,
    /// Add or manage comments.
    Comment,
    /// Render the review state for consumption.
    Render,
}

/// Arguments for `wiff new`.
#[derive(Debug, Args)]
struct NewArgs {
    /// Diff the index against HEAD (`git diff --cached`) instead of the working
    /// tree.
    #[arg(long)]
    cached: bool,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
    /// Create the session without launching the review TUI.
    #[arg(long)]
    no_tui: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::New(args) => run_new(args).await,
        Command::Resume
        | Command::Session
        | Command::Refresh
        | Command::Comment
        | Command::Render => {
            bail!("not yet implemented");
        }
    }
}

/// Create a session: capture a diff from git or piped stdin, persist it, and
/// report where it landed.
async fn run_new(args: NewArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let identity = ProjectIdentity::for_dir_or_forced(&cwd, args.project.as_deref())?;
    let captured = capture_source(&args, &identity).await?;
    if captured.text.trim().is_empty() {
        bail!("no changes to review");
    }
    let base = data_dir()?;
    let log = create_session(&base, &identity, &cwd, &captured)?;
    report_created(&log);
    if !args.no_tui {
        eprintln!("(the review TUI is not yet implemented; session created headlessly)");
    }
    Ok(())
}

/// Choose and run the diff source: a diff piped on stdin, else git.
async fn capture_source(
    args: &NewArgs,
    identity: &ProjectIdentity,
) -> anyhow::Result<CapturedDiff> {
    // A diff piped on stdin takes precedence over git: it is an explicit,
    // one-shot snapshot the caller supplied. Only content actually piped counts;
    // a non-terminal but empty stdin (a redirect, or a non-interactive harness)
    // falls through to git rather than yielding an empty snapshot.
    if let Some(text) = read_piped_stdin().await? {
        if args.cached {
            bail!("--cached diffs git and cannot be combined with a diff piped on stdin");
        }
        return Ok(CapturedDiff {
            text,
            source: SourceKind::Stdin,
        });
    }
    let root = identity.repo_root.clone().context(
        "no diff was piped on stdin and the current directory is not inside a repository",
    )?;
    let source = match identity.scm {
        Some(ScmType::Git) if args.cached => GitSource::index(root),
        Some(ScmType::Git) => GitSource::worktree(root),
        Some(other) => bail!(
            "{} is a {other} repository, which wiff cannot capture from yet; pipe a unified diff on stdin instead",
            root.display()
        ),
        None => bail!(
            "{} is not a recognized repository; pipe a unified diff on stdin instead",
            root.display()
        ),
    };
    Ok(source.capture().await?)
}

/// Read a diff piped on stdin, returning `None` when stdin is a terminal or
/// carries no content.
async fn read_piped_stdin() -> anyhow::Result<Option<String>> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    // Warn an interactive user that we are about to block on their input, so a
    // bare `wiff new` in a terminal-less-stdin situation does not look hung.
    if std::io::stderr().is_terminal() {
        eprintln!("Reading diff from stdin...");
    }
    let mut text = String::new();
    tokio::io::stdin()
        .read_to_string(&mut text)
        .await
        .context("could not read the diff from stdin")?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

/// Print where a freshly created session lives.
fn report_created(log: &SessionLog) {
    println!("created session {}", log.ulid());
    println!("  log: {}", log.path().display());
    println!("  sideband: {}", log.sideband_dir().display());
}
