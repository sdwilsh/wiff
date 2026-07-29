//! `wiff explore`: manage the file set of an explore review.
//!
//! An explore review annotates existing code, so its content is a set of files
//! rather than a change. `explore add` widens that set: it re-reads the current
//! files plus the requested ones at their working-copy state and captures the
//! result as a new version, through the content-hashed capture a refresh uses,
//! so re-adding a file already under review is a no-op.

use std::path::Path;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use wiff_core::record::{Author, SourceKind};
use wiff_core::review::ReviewState;
use wiff_core::{LockWait, SessionLog, widen_explore};

use super::{explore_root, resolve_author, resolve_session};

/// Arguments for `wiff explore`.
#[derive(Debug, Args)]
pub struct ExploreArgs {
    #[command(subcommand)]
    command: ExploreCommand,
}

/// The `wiff explore` subcommands.
#[derive(Debug, Subcommand)]
enum ExploreCommand {
    /// Add files to the review, capturing each at its current state.
    Add(AddArgs),
}

/// Arguments for `wiff explore add`.
#[derive(Debug, Args)]
struct AddArgs {
    /// The files to add, relative to the repository root or the current
    /// directory.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<String>,
    /// Add to a specific session by ULID instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
    /// Attribute the rebasing of existing comments to an agent rather than the
    /// human reviewer.
    #[arg(long)]
    agent: bool,
    /// Override the acting author's display name.
    #[arg(long)]
    author: Option<String>,
}

impl ExploreArgs {
    /// Run the selected `wiff explore` subcommand.
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            ExploreCommand::Add(args) => args.run(),
        }
    }
}

impl AddArgs {
    /// Widen the explore review's file set with the requested paths.
    fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let state = ReviewState::load(&path)?;
        if !matches!(state.session.source, SourceKind::Explore) {
            bail!(
                "session {} is not an explore review; `wiff explore add` applies only to a \
                 session created with `wiff new --explore`",
                state.session.ulid
            );
        }
        let root = explore_root(&state.session);
        let requested: Vec<String> = self
            .paths
            .iter()
            .map(|input| normalize_path(&root, input))
            .collect::<anyhow::Result<_>>()?;

        let author = resolve_author(self.agent, self.author.clone())?;
        let mut log = SessionLog::open(&path)?;
        match widen_explore(&mut log, &root, &requested, author, LockWait::Block)? {
            Some(outcome) => {
                let after = ReviewState::load(&path)?;
                let count = after.latest_version().map(|v| v.files.len()).unwrap_or(0);
                let plural = if count == 1 { "" } else { "s" };
                println!(
                    "captured v{}; {count} file{plural} under review",
                    outcome.version
                );
            }
            None => println!("no change; those files are already under review"),
        }
        Ok(())
    }
}

/// Add the file `file` (as the user spelled it) to the explore session at `path`
/// when it is not already under review, capturing the widened set at its
/// working-copy state and rebasing existing comments. Returns the file's
/// root-relative spelling, which a comment target must use to anchor into the
/// capture. Does nothing for a non-explore session, returning `file` unchanged.
/// Errors when the file cannot be read as text.
///
/// The widening version and the comment that follows it take the session lock in
/// turn rather than as one hold. The comment anchors against whatever version is
/// latest when it is appended, which is the version this widen just wrote, so a
/// comment on a freshly added file is recorded against a version that contains
/// it.
pub(crate) fn ensure_file_present(
    path: &Path,
    file: &str,
    author: Author,
) -> anyhow::Result<String> {
    let state = ReviewState::load(path)?;
    if !matches!(state.session.source, SourceKind::Explore) {
        return Ok(file.to_string());
    }
    let root = explore_root(&state.session);
    let normalized = normalize_path(&root, file)?;
    let mut log = SessionLog::open(path)?;
    widen_explore(
        &mut log,
        &root,
        std::slice::from_ref(&normalized),
        author,
        LockWait::Block,
    )?;
    Ok(normalized)
}

/// Express `input` as a path relative to the review's canonical `root`, using
/// forward slashes, so it matches the after-side paths a capture records. A path
/// given relative to the current directory is resolved against it first; one
/// that resolves outside the root is rejected. `root` is already canonical, so a
/// symlinked or dot-laden input strips against the same base the capture reads
/// under.
pub(crate) fn normalize_path(root: &Path, input: &str) -> anyhow::Result<String> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let input_path = Path::new(input);
    let absolute = if input_path.is_absolute() {
        input_path.to_path_buf()
    } else {
        cwd.join(input_path)
    };
    // Canonicalize the input where possible so a symlinked or dot-laden path
    // still strips cleanly; a not-yet-readable file keeps its resolved-against-cwd
    // form and is reported as unreadable by the capture instead.
    let absolute_real = absolute.canonicalize().unwrap_or(absolute);
    match absolute_real.strip_prefix(root) {
        Ok(relative) => Ok(to_slash(relative)),
        Err(_) => bail!("{input} is outside the review's root {}", root.display()),
    }
}

/// Join a path's components with forward slashes, the spelling a capture stores
/// and a comment target matches against.
pub(crate) fn to_slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{ExploreArgs, normalize_path};

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: ExploreArgs,
    }

    #[test]
    fn add_requires_at_least_one_path() {
        let error = TestCli::try_parse_from(["explore", "add"])
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn an_absolute_path_inside_the_root_becomes_root_relative() {
        let root = tempfile::tempdir().expect("root");
        // The review root is canonical in use, matching `explore_root`; canonicalize
        // here so the test exercises the same base the capture reads under.
        let root = root.path().canonicalize().expect("canonical root");
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("sub/foo.txt"), "x").expect("write");
        // An absolute path under the root reduces to the root-relative spelling a
        // capture stores and a comment target matches, with forward slashes.
        let input = root.join("sub/foo.txt").display().to_string();
        let normalized = normalize_path(&root, &input).expect("normalize");
        wince::assert_eq!(normalized, "sub/foo.txt".to_string());
    }

    #[test]
    fn a_path_outside_the_root_is_rejected() {
        let root = tempfile::tempdir().expect("root");
        let root = root.path().canonicalize().expect("canonical root");
        let outside = tempfile::tempdir().expect("outside");
        let outside = outside.path().canonicalize().expect("canonical outside");
        std::fs::write(outside.join("bar.txt"), "x").expect("write");
        let input = outside.join("bar.txt").display().to_string();
        let error = normalize_path(&root, &input).expect_err("outside root");
        wince::assert_eq!(
            format!("{error}"),
            format!(
                "{} is outside the review's root {}",
                outside.join("bar.txt").display(),
                root.display()
            )
        );
    }
}
