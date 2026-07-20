//! `wiff new`: create a review session from a captured diff.

use std::io::IsTerminal;

use anyhow::{Context, bail};
use clap::Args;
use wiff_config::Config;
use wiff_core::record::{Description, SourceKind};
use wiff_core::session::data_dir;
use wiff_core::{
    BaseRuleset, CapturedDiff, ProjectIdentity, SessionLog, create_session, parse_ruleset,
};

use super::{DiffSelection, capture_scm_diff, read_piped_stdin, resolve_author};
use crate::tui;

/// Arguments for `wiff new`.
#[derive(Debug, Args)]
pub struct NewArgs {
    /// Review the staged index instead of the working tree, against the same
    /// base.
    #[arg(long, conflicts_with = "change")]
    cached: bool,
    /// Review a branch, change, or revision against its first parent, like
    /// `git show`. A branch or change follows its newest commit on refresh; a
    /// bare revision is held.
    #[arg(long, value_name = "REF", conflicts_with = "from_base")]
    change: Option<String>,
    /// Review the whole branch back to its fork point, taking the base from the
    /// configured `base_revision_rules` instead of pinning at the current commit.
    #[arg(long)]
    from_base: bool,
    /// Review against an explicit base ruleset, overriding the default base and
    /// `--from-base`; `--base empty` reviews the whole history to the root.
    #[arg(long, value_name = "RULESET", conflicts_with = "from_base")]
    base: Option<String>,
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
    /// Whether this invocation will launch the review TUI once the session is
    /// created.
    pub fn opens_tui(&self) -> bool {
        !self.no_tui
    }

    /// Create a session: capture a diff from git or piped stdin, persist it, and
    /// report where it landed.
    pub async fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir().context("could not determine the current directory")?;
        let identity = ProjectIdentity::for_dir_or_forced(&cwd, self.project.as_deref())?;
        let config = Config::load()?;
        let captured = self.capture_source(&identity, &config).await?;
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
        tui::open(log.path(), &config, false)
    }

    /// Choose and run the diff source: a diff piped on stdin, else git.
    async fn capture_source(
        &self,
        identity: &ProjectIdentity,
        config: &Config,
    ) -> anyhow::Result<CapturedDiff> {
        // A named git selection and a piped diff pull in opposite directions.
        // Detect a piped diff by its non-terminal stdin before committing to a
        // (blocking) read, so the conflict is reported at once rather than after
        // waiting on input.
        if self.selects_git() {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "a diff piped on stdin cannot be combined with --cached, --change, --from-base, or --base"
                );
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
        capture_scm_diff(identity.scm, root, self.selection(), self.base(config)?).await
    }

    /// Whether a flag naming a specific git selection was given, as opposed to
    /// the default working tree that a piped diff may stand in for.
    fn selects_git(&self) -> bool {
        self.cached || self.change.is_some() || self.from_base || self.base.is_some()
    }

    /// The slice of the repository to capture from the chosen flags: a named
    /// change, the staged index, or the working tree by default.
    fn selection(&self) -> DiffSelection {
        if let Some(change) = &self.change {
            DiffSelection::Change(change.clone())
        } else if self.cached {
            DiffSelection::Staged
        } else {
            DiffSelection::Worktree
        }
    }

    /// The base ruleset chosen on the command line, or `None` to take the
    /// selection's default: an explicit `--base` ruleset, else the configured
    /// `base_revision_rules` under `--from-base`.
    fn base(&self, config: &Config) -> anyhow::Result<Option<BaseRuleset>> {
        if let Some(text) = &self.base {
            parse_ruleset(text).with_context(|| format!("invalid --base ruleset '{text}'"))?;
            Ok(Some(BaseRuleset::new(text.clone())))
        } else if self.from_base {
            let rules = &config.base_revision_rules;
            parse_ruleset(rules.as_str())
                .with_context(|| format!("invalid base_revision_rules '{rules}' in config"))?;
            Ok(Some(rules.clone()))
        } else {
            Ok(None)
        }
    }
}

/// Print where a freshly created session lives.
fn report_created(log: &SessionLog) {
    println!("created session {}", log.ulid());
    println!("  log: {}", log.path().display());
    println!("  sideband: {}", log.sideband_dir().display());
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// A parser wrapper so the `wiff new` flags can be built from an argv in a
    /// test without reconstructing every field by hand.
    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: NewArgs,
    }

    fn args_from(argv: &[&str]) -> NewArgs {
        TestCli::parse_from(std::iter::once("new").chain(argv.iter().copied())).args
    }

    #[test]
    fn no_base_flag_takes_the_selection_default() {
        let base = args_from(&[]).base(&Config::default()).expect("base");
        wince::assert_eq!(base, None);
    }

    #[test]
    fn an_explicit_base_ruleset_is_used_verbatim() {
        let base = args_from(&["--base", "empty"])
            .base(&Config::default())
            .expect("base");
        wince::assert_eq!(base, Some(BaseRuleset::new("empty")));
    }

    #[test]
    fn a_malformed_base_ruleset_is_a_contextualized_error() {
        let error = args_from(&["--base", "not a ( valid"])
            .base(&Config::default())
            .expect_err("invalid base");
        wince::assert_eq!(
            format!("{error:#}"),
            "invalid --base ruleset 'not a ( valid': invalid base ruleset at position 0: \
             unknown base operator 'not'; use ref, parent, merge-base, empty, or an scm-native \
             expression (git, jj, hg, or sl)"
                .to_string()
        );
    }

    #[test]
    fn from_base_uses_the_configured_ruleset() {
        let config = Config {
            base_revision_rules: BaseRuleset::new("merge-base(trunk)"),
            ..Config::default()
        };
        let base = args_from(&["--from-base"]).base(&config).expect("base");
        wince::assert_eq!(base, Some(BaseRuleset::new("merge-base(trunk)")));
    }

    #[test]
    fn a_malformed_config_ruleset_is_a_contextualized_error() {
        let config = Config {
            base_revision_rules: BaseRuleset::new("not a ( valid"),
            ..Config::default()
        };
        let error = args_from(&["--from-base"])
            .base(&config)
            .expect_err("invalid config ruleset");
        wince::assert_eq!(
            format!("{error:#}"),
            "invalid base_revision_rules 'not a ( valid' in config: invalid base ruleset at \
             position 0: unknown base operator 'not'; use ref, parent, merge-base, empty, or an \
             scm-native expression (git, jj, hg, or sl)"
                .to_string()
        );
    }
}
