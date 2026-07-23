//! The `wiff` command-line entry point.

mod command;
mod logging;
mod render;
mod tui;

#[cfg(test)]
pub(crate) mod testutil;

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
    let cli = Cli::parse();
    logging::init();
    cli.command.run().await
}
