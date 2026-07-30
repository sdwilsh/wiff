//! `wiff refresh`: capture a new diff version into a session and rebase its
//! comments onto it.

use anyhow::bail;
use clap::Args;
use wiff_core::record::SourceKind;
use wiff_core::review::ReviewState;
use wiff_core::{
    CapturedDiff, LockWait, RefreshOutcome, SessionLog, refresh_session, widen_explore,
};

use super::{explore_root, read_piped_stdin, recapture_diff, resolve_author, resolve_session};

/// Arguments for `wiff refresh`.
#[derive(Debug, Args)]
pub struct RefreshArgs {
    /// Refresh a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
    /// Attribute the reanchoring to an agent rather than the human reviewer.
    #[arg(long)]
    agent: bool,
    /// Override the acting author's display name.
    #[arg(long)]
    author: Option<String>,
}

impl RefreshArgs {
    /// Recapture the session's source, append it as a new version, and report
    /// how its comments rebased.
    pub async fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let state = ReviewState::load(&path)?;
        let author = resolve_author(self.agent, self.author)?;
        let mut log = SessionLog::open(&path)?;
        // An explore review re-reads its file set from the latest version under
        // the session lock rather than recapturing an scm range; widening with
        // no new paths is that re-read.
        let outcome = if matches!(state.session.source, SourceKind::Explore) {
            let root = explore_root(&state.session);
            widen_explore(&mut log, &root, &[], author, LockWait::Block)?
        } else {
            let captured = recapture(&state).await?;
            refresh_session(&mut log, &captured, author, LockWait::Block)?
        };
        match outcome {
            Some(outcome) => report(&outcome),
            None => {
                let current = state.latest_version().map(|v| v.number.get()).unwrap_or(0);
                println!("no changes since v{current}");
            }
        }
        Ok(())
    }
}

/// Recapture the diff from the session's original source: rerun the SCM for a
/// regenerable source, or read a fresh diff piped on stdin for a stdin source.
async fn recapture(state: &ReviewState) -> anyhow::Result<CapturedDiff> {
    if let Some(captured) = recapture_diff(state).await? {
        return Ok(captured);
    }
    match read_piped_stdin().await? {
        Some(text) => Ok(CapturedDiff {
            text,
            source: SourceKind::Stdin,
            base_revision: None,
            base_tip_relative: false,
            head_revision: None,
        }),
        None => bail!("this session's diff came from stdin; pipe the new diff on stdin"),
    }
}

/// Print the captured version and the tally of rebased comments, warning on
/// stderr when the review's base moved out from under it.
fn report(outcome: &RefreshOutcome) {
    if let Some(shift) = &outcome.base_shift {
        eprintln!(
            "warning: the review's base moved from {} to {}; it now starts from a different commit",
            shift.from, shift.to,
        );
    }
    let total = outcome.exact + outcome.approximate + outcome.relocated + outcome.outdated;
    println!(
        "captured v{}; rebased {total} comment{}: {} exact, {} shifted, {} moved, {} outdated",
        outcome.version,
        if total == 1 { "" } else { "s" },
        outcome.exact,
        outcome.approximate,
        outcome.relocated,
        outcome.outdated,
    );
}
