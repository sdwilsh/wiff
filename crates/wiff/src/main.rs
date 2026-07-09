//! The `wiff` command-line entry point.

mod command;
mod render;
mod tui;

use clap::Parser;

use crate::command::Command;

/// wiff: sniff out the wiff in your diff, from the comfort of your terminal.
#[derive(Debug, Parser)]
#[command(name = "wiff", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Cli::parse().command.run().await
}
