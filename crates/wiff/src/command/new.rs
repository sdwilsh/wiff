//! `wiff new`: create a review session from a captured diff.

use std::io::IsTerminal;

use anyhow::{Context, bail};
use clap::{ArgGroup, Args};
use wiff_config::Config;
use wiff_core::record::{Description, SourceKind};
use wiff_core::session::data_dir;
use wiff_core::{CapturedDiff, ProjectIdentity, SessionLog, create_session};

use super::{DiffSelection, capture_scm_diff, read_piped_stdin, resolve_author};
use crate::tui;

/// Arguments for `wiff new`.
#[derive(Debug, Args)]
#[command(group(ArgGroup::new("scope").args(["cached", "rev", "head"])))]
pub struct NewArgs {
    /// Diff the index against HEAD (`git diff --cached`) instead of the working
    /// tree.
    #[arg(long)]
    cached: bool,
    /// Review the changes a revision introduces, like `git show REF`.
    #[arg(long, value_name = "REF")]
    rev: Option<String>,
    /// Review the changes the latest commit introduces; sugar for `--rev HEAD`.
    #[arg(long)]
    head: bool,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
    /// Set the review's description, a commit-message-shaped title and body
    /// (the first line is the title, the rest the body).
    #[arg(long, value_name = "TEXT")]
    description: Option<String>,
    /// The description author's display name.
    #[arg(long, requires = "description")]
    author: Option<String>,
    /// Attribute the initial description to an agent rather than a human.
    #[arg(long, requires = "description")]
    agent: bool,
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
        let description = match &self.description {
            Some(text) => {
                let author = resolve_author(self.agent, self.author.clone())?;
                Some((author, Description::from_message(text)))
            }
            None => None,
        };
        let base = data_dir()?;
        let log = create_session(&base, &identity, &cwd, &captured, description)?;
        report_created(&log);
        if self.no_tui {
            return Ok(());
        }
        let config = Config::load()?;
        tui::open(log.path(), &config, false)
    }

    /// Choose and run the diff source: a diff piped on stdin, else git.
    async fn capture_source(&self, identity: &ProjectIdentity) -> anyhow::Result<CapturedDiff> {
        // A named git selection and a piped diff pull in opposite directions.
        // Detect a piped diff by its non-terminal stdin before committing to a
        // (blocking) read, so the conflict is reported at once rather than after
        // waiting on input.
        if self.selects_git() {
            if !std::io::stdin().is_terminal() {
                bail!("a diff piped on stdin cannot be combined with --cached, --rev, or --head");
            }
        } else if let Some(text) = read_piped_stdin().await? {
            // A diff piped on stdin takes precedence over git: it is an
            // explicit, one-shot snapshot the caller supplied.
            return Ok(CapturedDiff {
                text,
                source: SourceKind::Stdin,
                base_revision: None,
                base_tip_relative: false,
                head_revision: None,
            });
        }
        let root = identity.repo_root.clone().context(
            "no diff was piped on stdin and the current directory is not inside a repository",
        )?;
        capture_scm_diff(identity.scm, root, self.selection()).await
    }

    /// Whether a flag naming a specific git selection was given, as opposed to
    /// the default working tree that a piped diff may stand in for.
    fn selects_git(&self) -> bool {
        self.cached || self.head || self.rev.is_some()
    }

    /// The slice of the repository to capture from the chosen flags: a named
    /// revision, the staged index, or the working tree by default.
    fn selection(&self) -> DiffSelection {
        if let Some(rev) = &self.rev {
            DiffSelection::Rev(rev.clone())
        } else if self.head {
            DiffSelection::Rev("HEAD".to_string())
        } else if self.cached {
            DiffSelection::Staged
        } else {
            DiffSelection::Worktree
        }
    }
}

/// Print where a freshly created session lives.
fn report_created(log: &SessionLog) {
    println!("created session {}", log.ulid());
    println!("  log: {}", log.path().display());
    println!("  sideband: {}", log.sideband_dir().display());
}
