//! `wiff new`: create a review session from a captured diff.

use anyhow::{Context, bail};
use clap::Args;
use wiff_core::record::SourceKind;
use wiff_core::session::data_dir;
use wiff_core::{CapturedDiff, ProjectIdentity, SessionLog, create_session};

use super::{capture_scm_diff, read_piped_stdin};

/// Arguments for `wiff new`.
#[derive(Debug, Args)]
pub struct NewArgs {
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

impl NewArgs {
    /// Create a session: capture a diff from git or piped stdin, persist it, and
    /// report where it landed.
    pub async fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir().context("could not determine the current directory")?;
        let identity = ProjectIdentity::for_dir_or_forced(&cwd, self.project.as_deref())?;
        let captured = self.capture_source(&identity).await?;
        let base = data_dir()?;
        let log = create_session(&base, &identity, &cwd, &captured)?;
        report_created(&log);
        if !self.no_tui {
            eprintln!("(the review TUI is not yet implemented; session created headlessly)");
        }
        Ok(())
    }

    /// Choose and run the diff source: a diff piped on stdin, else git.
    async fn capture_source(&self, identity: &ProjectIdentity) -> anyhow::Result<CapturedDiff> {
        // A diff piped on stdin takes precedence over git: it is an explicit,
        // one-shot snapshot the caller supplied.
        if let Some(text) = read_piped_stdin().await? {
            if self.cached {
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
        capture_scm_diff(identity.scm, root, self.cached).await
    }
}

/// Print where a freshly created session lives.
fn report_created(log: &SessionLog) {
    println!("created session {}", log.ulid());
    println!("  log: {}", log.path().display());
    println!("  sideband: {}", log.sideband_dir().display());
}
