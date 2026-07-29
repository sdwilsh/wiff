//! `wiff resume`: reopen an existing session in the review TUI.

use std::path::PathBuf;

use clap::Args;
use wiff_config::Config;
use wiff_forge::TokenOverride;

use super::resolve_session;
use crate::tui;

/// Arguments for `wiff resume`.
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Resume a specific session by ULID instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
    /// Read the forge token from this file, used when publishing to the bound
    /// pull request. Uses its trimmed contents.
    #[arg(long)]
    forge_token_file: Option<PathBuf>,
    /// The forge token, given directly, used when publishing to the bound pull
    /// request.
    #[arg(long, conflicts_with = "forge_token_file")]
    forge_token: Option<String>,
}

impl ResumeArgs {
    /// Reopen the resolved session in the review TUI.
    pub fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let config = Config::load()?;
        let cli = TokenOverride {
            token_file: self.forge_token_file,
            token: self.forge_token,
        };
        tui::open(&path, &config, &cli, true)
    }
}
