//! `wiff new`: create a review session from a captured diff.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, bail};
use clap::{ArgGroup, Args};
use ulid::Ulid;
use wiff_config::Config;
use wiff_core::record::{Author, Description, SourceKind};
use wiff_core::session::data_dir;
use wiff_core::{
    BaseRuleset, CapturedDiff, IfNeeded, ProjectIdentity, RefreshOutcome, SessionLog,
    capture_explore, create_session, parse_ruleset, reuse_or_create,
};
use wiff_forge::TokenOverride;

use super::{DiffSelection, capture_scm_diff, read_piped_stdin, resolve_author};
use crate::tui;

/// Arguments for `wiff new`.
#[derive(Debug, Args)]
#[command(group(
    // `--author`/`--agent` attribute either the initial description or a
    // refresh's rebased comments, so one of those two flags must accompany them.
    ArgGroup::new("attributable").args(["description", "if_needed"]).multiple(true)
))]
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
    /// Open an empty review over existing code rather than a change. Add files to
    /// it with `wiff explore add` or the in-review file picker; each is captured
    /// at its current state with no change implied.
    #[arg(
        long,
        conflicts_with_all = ["cached", "change", "from_base", "base", "if_needed"]
    )]
    explore: bool,
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
    /// The acting author's display name: the initial description's author, and
    /// under `--if-needed` who a refresh's rebased comments are attributed to.
    #[arg(long, requires = "attributable")]
    author: Option<String>,
    /// Act as an agent rather than a human when attributing the description and,
    /// under `--if-needed`, a refresh's rebased comments.
    #[arg(long, requires = "attributable")]
    agent: bool,
    /// Create the session without launching the review TUI.
    #[arg(long)]
    no_tui: bool,
    /// Reuse the project's session for this same range when one exists: refresh
    /// it in place if the working copy has moved on, leave it untouched when it
    /// already matches, and create a session only when none does. Intended for
    /// automation maintaining a review; requires `--no-tui`.
    #[arg(long, requires = "no_tui")]
    if_needed: bool,
}

impl NewArgs {
    /// Create a session: capture a diff from git or piped stdin, persist it, and
    /// report where it landed.
    pub async fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir().context("could not determine the current directory")?;
        let identity = ProjectIdentity::for_dir_or_forced(&cwd, self.project.as_deref())?;
        let config = Config::load()?;
        let captured = self.capture_source(&identity, &config).await?;
        if self.if_needed {
            return self.ensure_session(&identity, &cwd, &captured);
        }
        // An explore session opens empty by design, so the empty-capture check
        // that guards a change review does not apply to it.
        if !self.explore && captured.text.trim().is_empty() {
            bail!("no changes to review");
        }
        let base = data_dir()?;
        let log = create_session(&base, &identity, &cwd, &captured, self.description()?)?;
        report_created(&log);
        if self.no_tui {
            return Ok(());
        }
        // A freshly captured session is never bound to a pull request, so the
        // publish flow has no forge to reach; the default overrides suffice.
        tui::open(log.path(), &config, &TokenOverride::default(), false)
    }

    /// Satisfy `--if-needed`, reporting what it did.
    fn ensure_session(
        &self,
        identity: &ProjectIdentity,
        cwd: &Path,
        captured: &CapturedDiff,
    ) -> anyhow::Result<()> {
        let base = data_dir()?;
        let author = resolve_author(self.agent, self.author.clone())?;
        match reuse_or_create(&base, identity, cwd, captured, author, self.description()?)? {
            IfNeeded::Created(log) => report_created(&log),
            IfNeeded::Unchanged(log) => {
                warn_description_ignored(self.description.as_deref());
                report_unchanged(log.ulid());
            }
            IfNeeded::Refreshed(log, outcome) => {
                warn_description_ignored(self.description.as_deref());
                report_refreshed(log.ulid(), &outcome);
            }
            // No session covers this range and there is nothing to open one
            // from, the same dead end a plain `wiff new` reports on a clean tree.
            IfNeeded::NothingToReview => bail!("no changes to review"),
        }
        Ok(())
    }

    /// The initial description and its resolved author, when `--description` was
    /// given.
    fn description(&self) -> anyhow::Result<Option<(Author, Description)>> {
        match &self.description {
            Some(text) => {
                let author = resolve_author(self.agent, self.author.clone())?;
                Ok(Some((author, Description::from_message(text))))
            }
            None => Ok(None),
        }
    }

    /// Choose and run the diff source: a diff piped on stdin, else git.
    async fn capture_source(
        &self,
        identity: &ProjectIdentity,
        config: &Config,
    ) -> anyhow::Result<CapturedDiff> {
        // An explore session starts with no files; its capture is an empty
        // all-context diff that later adds widen. With no files to read, the
        // root only matters once files are added, so a repo-less session falls
        // back to the current directory here.
        if self.explore {
            let root = identity
                .repo_root
                .clone()
                .unwrap_or_else(|| Path::new(".").to_path_buf());
            return Ok(capture_explore(&root, &[]).captured);
        }
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
            DiffSelection::WorkingCopy
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

/// Warn on stderr that a `--description` given alongside `--if-needed` was
/// ignored, since it only seeds a freshly created session and a reused one keeps
/// its own description.
fn warn_description_ignored(description: Option<&str>) {
    if description.is_some() {
        eprintln!(
            "warning: --description was ignored; it seeds a new session and this run reused an \
             existing one"
        );
    }
}

/// Report that `--if-needed` left a matching session untouched because it already
/// describes the current state.
fn report_unchanged(ulid: Ulid) {
    println!("session {ulid} already describes the current state");
}

/// Report that `--if-needed` refreshed a matching session in place, warning on
/// stderr when the review's base moved and tallying how its comments rebased.
fn report_refreshed(ulid: Ulid, outcome: &RefreshOutcome) {
    if let Some(shift) = &outcome.base_shift {
        eprintln!(
            "warning: the review's base moved from {} to {}; it now starts from a different commit",
            shift.from, shift.to,
        );
    }
    let total = outcome.exact + outcome.approximate + outcome.relocated + outcome.outdated;
    println!(
        "refreshed session {ulid}: captured v{}; rebased {total} comment{}: {} exact, {} shifted, \
         {} moved, {} outdated",
        outcome.version,
        if total == 1 { "" } else { "s" },
        outcome.exact,
        outcome.approximate,
        outcome.relocated,
        outcome.outdated,
    );
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

    #[test]
    fn if_needed_requires_no_tui() {
        let error = TestCli::try_parse_from(["new", "--if-needed"])
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn author_without_a_description_or_if_needed_is_rejected() {
        let error = TestCli::try_parse_from(["new", "--author", "wez"])
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn explore_conflicts_with_a_git_selection() {
        let error = TestCli::try_parse_from(["new", "--explore", "--cached"])
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn explore_is_accepted_with_no_tui() {
        let args = args_from(&["--explore", "--no-tui"]);
        wince::assert_eq!((args.explore, args.no_tui), (true, true));
    }

    #[test]
    fn author_is_accepted_alongside_if_needed() {
        let args = args_from(&["--no-tui", "--if-needed", "--author", "wez"]);
        wince::assert_eq!(
            (args.if_needed, args.no_tui, args.author),
            (true, true, Some("wez".to_string()))
        );
    }
}
