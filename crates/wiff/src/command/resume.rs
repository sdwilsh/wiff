//! `wiff resume`: reopen an existing session in the review TUI.

use clap::Args;
use wiff_config::Config;

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
}

impl ResumeArgs {
    /// Reopen the resolved session in the review TUI.
    pub fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let config = Config::load()?;
        tui::open(&path, &config)
    }
}
