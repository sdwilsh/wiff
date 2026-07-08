//! The `wiff` command-line entry point.

use clap::{Parser, Subcommand};

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
    New,
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::New
        | Command::Resume
        | Command::Session
        | Command::Refresh
        | Command::Comment
        | Command::Render => {
            anyhow::bail!("not yet implemented");
        }
    }
}
