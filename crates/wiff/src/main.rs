//! The `wiff` command-line entry point.

mod command;
mod render;
mod tui;

#[cfg(test)]
pub(crate) mod testutil;

use clap::Parser;
use tracing_subscriber::EnvFilter;

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
    // The TUI owns the terminal, so log output would corrupt its alternate
    // screen; a subscriber is installed only for the command-line commands that
    // leave the terminal to us. The default filter warns and above; RUST_LOG
    // overrides it.
    if !cli.command.opens_tui() {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .without_time()
            .with_target(false)
            .init();
    }
    cli.command.run().await
}
