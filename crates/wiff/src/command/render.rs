//! `wiff render`: emit a session's folded review state for consumption.

use clap::Args;
use wiff_core::review::ReviewState;

use super::resolve_session;
use crate::render::{Format, render};

/// Arguments for `wiff render`.
#[derive(Debug, Args)]
pub struct RenderArgs {
    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    format: Format,
    /// Render a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl RenderArgs {
    /// Render a session's folded review state to stdout.
    pub fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let state = ReviewState::load(&path)?;
        print!("{}", render(&state, self.format)?);
        Ok(())
    }
}
