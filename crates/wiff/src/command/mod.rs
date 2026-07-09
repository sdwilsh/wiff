//! The `wiff` subcommands: one module per command, each owning its arguments
//! and its `run` entry point. This module wires them into the top-level
//! [`Command`] enum and holds the few helpers shared across commands.

mod comment;
mod new;
mod render;
mod session;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Subcommand;
use tokio::io::AsyncReadExt;
use ulid::Ulid;
use wiff_core::ProjectIdentity;
use wiff_core::session::{active_session, data_dir, session_file};

use self::comment::CommentArgs;
use self::new::NewArgs;
use self::render::RenderArgs;
use self::session::SessionArgs;

/// The top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a review session from a source and open it.
    New(NewArgs),
    /// Resume an existing review session.
    Resume,
    /// Manage sessions.
    Session(SessionArgs),
    /// Capture a new diff version into a session and rebase comments.
    Refresh,
    /// Add or manage comments.
    Comment(CommentArgs),
    /// Render the review state for consumption.
    Render(RenderArgs),
}

impl Command {
    /// Run the selected subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        match self {
            Command::New(args) => args.run().await,
            Command::Comment(args) => args.run().await,
            Command::Render(args) => args.run(),
            Command::Session(args) => args.run(),
            Command::Resume | Command::Refresh => {
                bail!("not yet implemented");
            }
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
