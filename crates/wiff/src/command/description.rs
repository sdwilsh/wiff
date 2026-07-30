//! `wiff description`: set or show a session's description.

use std::path::Path;

use clap::{Args, Subcommand};
use wiff_core::record::Description;
use wiff_core::review::ReviewState;
use wiff_core::{LockWait, SessionLog, set_description};

use super::{read_piped_stdin, resolve_author, resolve_session};

/// Arguments for `wiff description`.
#[derive(Debug, Args)]
pub struct DescriptionArgs {
    #[command(subcommand)]
    command: DescriptionCommand,
}

impl DescriptionArgs {
    /// Dispatch the selected `wiff description` subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            DescriptionCommand::Set(args) => args.run().await,
            DescriptionCommand::Show(args) => args.run(),
        }
    }
}

/// The `wiff description` subcommands.
#[derive(Debug, Subcommand)]
enum DescriptionCommand {
    /// Set the description from an argument or piped stdin.
    Set(DescriptionSetArgs),
    /// Show the current description.
    Show(DescriptionShowArgs),
}

/// Arguments for `wiff description set`.
#[derive(Debug, Args)]
struct DescriptionSetArgs {
    /// The description text, as a commit message: the first line is the title,
    /// the rest the body. When omitted, it is read from stdin.
    message: Option<String>,
    /// The author's display name.
    #[arg(long)]
    author: Option<String>,
    /// Attribute the description to an agent rather than a human.
    #[arg(long)]
    agent: bool,
    /// Target a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl DescriptionSetArgs {
    /// Set the session's description from the argument or piped stdin.
    async fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        let Some(message) = self.message().await? else {
            anyhow::bail!("provide the description text as an argument or on stdin");
        };
        let author = resolve_author(self.agent, self.author.clone())?;
        let description = Description::from_message(&message);
        let mut log = SessionLog::open(&path)?;
        set_description(&mut log, description, author, LockWait::Block)?;
        Ok(())
    }

    /// The description text: the positional argument, else piped stdin, else
    /// `None` when neither is present.
    async fn message(&self) -> anyhow::Result<Option<String>> {
        if let Some(text) = &self.message {
            return Ok(Some(text.clone()));
        }
        read_piped_stdin().await
    }
}

/// Arguments for `wiff description show`.
#[derive(Debug, Args)]
struct DescriptionShowArgs {
    /// Target a specific session by id instead of the active one.
    #[arg(long)]
    session: Option<String>,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl DescriptionShowArgs {
    /// Print the session's current description, or note its absence.
    fn run(self) -> anyhow::Result<()> {
        let path = resolve_session(self.session.as_deref(), self.project.as_deref())?;
        show(&path)
    }
}

/// Print the session's current description, or note its absence.
fn show(path: &Path) -> anyhow::Result<()> {
    let state = ReviewState::load(path)?;
    println!("{}", describe_output(&state));
    Ok(())
}

/// The current description as its commit-message text, or a marker when the
/// session has none.
fn describe_output(state: &ReviewState) -> String {
    match &state.description {
        Some(description) => description.content.to_message(),
        None => "(no description)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wiff_core::record::{Author, AuthorKind, Description, SourceKind};
    use wiff_core::review::ReviewState;
    use wiff_core::{
        CapturedDiff, LockWait, ProjectIdentity, SessionLog, create_session, set_description,
    };

    use super::describe_output;

    const DIFF: &str = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -1 +1 @@
-old
+new
";

    fn identity() -> ProjectIdentity {
        ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: None,
            scm: None,
        }
    }

    fn session(base: &Path) -> SessionLog {
        let captured = CapturedDiff {
            text: DIFF.to_string(),
            source: SourceKind::Stdin,
            base_revision: None,
            base_tip_relative: false,
            head_revision: None,
        };
        create_session(base, &identity(), Path::new("/work"), &captured, None).unwrap()
    }

    fn author() -> Author {
        Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        }
    }

    #[test]
    fn showing_a_session_with_no_description_notes_its_absence() {
        let base = tempfile::tempdir().unwrap();
        let log = session(base.path());
        let state = ReviewState::load(log.path()).unwrap();
        wince::assert_eq!(describe_output(&state), "(no description)".to_string());
    }

    #[test]
    fn showing_a_set_description_prints_its_commit_message_form() {
        let base = tempfile::tempdir().unwrap();
        let mut log = session(base.path());
        set_description(
            &mut log,
            Description {
                title: "Tidy the parser".to_string(),
                body: "Split the lexer out.".to_string(),
            },
            author(),
            LockWait::Block,
        )
        .unwrap();
        let state = ReviewState::load(log.path()).unwrap();
        wince::assert_eq!(
            describe_output(&state),
            "Tidy the parser\n\nSplit the lexer out.".to_string()
        );
    }

    #[test]
    fn setting_a_description_twice_shows_only_the_latest() {
        let base = tempfile::tempdir().unwrap();
        let mut log = session(base.path());
        set_description(
            &mut log,
            Description {
                title: "First".to_string(),
                body: String::new(),
            },
            author(),
            LockWait::Block,
        )
        .unwrap();
        set_description(
            &mut log,
            Description {
                title: "Second".to_string(),
                body: "with a body".to_string(),
            },
            author(),
            LockWait::Block,
        )
        .unwrap();
        let state = ReviewState::load(log.path()).unwrap();
        wince::assert_eq!(describe_output(&state), "Second\n\nwith a body".to_string());
    }
}
