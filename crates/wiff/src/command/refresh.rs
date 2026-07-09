//! `wiff refresh`: capture a new diff version into a session and rebase its
//! comments onto it.

use anyhow::{Context, bail};
use clap::Args;
use wiff_core::record::{SessionHeader, SourceKind};
use wiff_core::review::ReviewState;
use wiff_core::{RefreshOutcome, ScmType, SessionLog, refresh_session};

use super::{capture_scm_diff, read_piped_stdin, resolve_session};

/// Arguments for `wiff refresh`.
#[derive(Debug, Args)]
pub struct RefreshArgs {
    /// Refresh a specific session by ULID instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl RefreshArgs {
    /// Recapture the session's source, append it as a new version, and report
    /// how its comments rebased.
    pub async fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let state = ReviewState::load(&path)?;
        let diff_text = recapture(&state.session).await?;
        let mut log = SessionLog::open(&path)?;
        match refresh_session(&mut log, &diff_text)? {
            Some(outcome) => report(&outcome),
            None => {
                let current = state.latest_version().map(|v| v.number).unwrap_or(0);
                println!("no changes since v{current}");
            }
        }
        Ok(())
    }
}

/// Recapture the diff from the session's original source: rerun git for a git
/// source, or read a fresh diff piped on stdin for a stdin source.
async fn recapture(header: &SessionHeader) -> anyhow::Result<String> {
    match header.source {
        SourceKind::GitWorktree | SourceKind::GitIndex => {
            let root = header.repo_root.clone().context(
                "the session records no repository root, so its git diff cannot be recaptured",
            )?;
            let captured = capture_scm_diff(
                Some(ScmType::Git),
                root.into(),
                header.source == SourceKind::GitIndex,
            )
            .await?;
            Ok(captured.text)
        }
        SourceKind::Stdin => match read_piped_stdin().await? {
            Some(text) => Ok(text),
            None => bail!("this session's diff came from stdin; pipe the new diff on stdin"),
        },
    }
}

/// Print the captured version and the tally of rebased comments.
fn report(outcome: &RefreshOutcome) {
    let total = outcome.exact + outcome.approximate + outcome.outdated;
    println!(
        "captured v{}; rebased {total} comment{}: {} exact, {} shifted, {} outdated",
        outcome.version,
        if total == 1 { "" } else { "s" },
        outcome.exact,
        outcome.approximate,
        outcome.outdated,
    );
}
