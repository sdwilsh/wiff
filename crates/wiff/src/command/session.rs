//! `wiff session`: list and remove review sessions.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use ulid::Ulid;
use wiff_core::ProjectIdentity;
use wiff_core::record::SourceKind;
use wiff_core::review::ReviewState;
use wiff_core::session::{
    active_session, data_dir, list_projects, list_sessions, remove_session, session_file,
};

/// Arguments for `wiff session`.
#[derive(Debug, Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    command: SessionCommand,
}

impl SessionArgs {
    /// Dispatch the selected `wiff session` subcommand.
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            SessionCommand::List(args) => args.run(),
            SessionCommand::Rm(args) => args.run(),
        }
    }
}

/// The `wiff session` subcommands.
#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List sessions for the current project, or across all projects.
    List(SessionListArgs),
    /// Remove a session, deleting its log and sideband directory.
    Rm(SessionRmArgs),
}

/// Arguments for `wiff session list`.
#[derive(Debug, Args)]
struct SessionListArgs {
    /// List sessions across every project rather than just the current one.
    #[arg(long)]
    all: bool,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl SessionListArgs {
    /// Print a summary of the matching sessions.
    fn run(self) -> anyhow::Result<()> {
        let base = data_dir()?;
        let projects = if self.all {
            list_projects(&base)?
        } else {
            let cwd =
                std::env::current_dir().context("could not determine the current directory")?;
            let identity = ProjectIdentity::for_dir_or_forced(&cwd, self.project.as_deref())?;
            vec![identity.canonical]
        };
        let mut groups = Vec::new();
        for project in projects {
            let rows = session_rows(&base, &project)?;
            if !rows.is_empty() {
                groups.push((project, rows));
            }
        }
        print!("{}", render_list(&groups, self.all));
        Ok(())
    }
}

/// Arguments for `wiff session rm`.
#[derive(Debug, Args)]
struct SessionRmArgs {
    /// The id of the session to remove.
    id: String,
    /// Force the project bucket name when it cannot be derived from the cwd.
    #[arg(long)]
    project: Option<String>,
}

impl SessionRmArgs {
    /// Remove a session and report the outcome.
    fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir().context("could not determine the current directory")?;
        let identity = ProjectIdentity::for_dir_or_forced(&cwd, self.project.as_deref())?;
        let base = data_dir()?;
        let ulid = Ulid::from_string(&self.id)
            .with_context(|| format!("{} is not a valid session id", self.id))?;
        let path = session_file(&base, &identity.canonical, ulid);
        if !path.exists() {
            bail!("no session {ulid} in project {}", identity.canonical);
        }
        remove_session(&path)?;
        println!("removed session {ulid}");
        Ok(())
    }
}

/// A one-line summary of a session for the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRow {
    /// The session's identity.
    ulid: Ulid,
    /// How its diff was captured.
    source: SourceKind,
    /// Whether it is the project's active (most recent) session.
    active: bool,
    /// The number of live (non-withdrawn) comments.
    comments: usize,
    /// How many of those are still open (unresolved).
    open: usize,
}

/// Summarize a project's sessions, most recent first, marking the active one.
fn session_rows(base: &std::path::Path, project: &str) -> anyhow::Result<Vec<SessionRow>> {
    let active = active_session(base, project).ok();
    let mut rows = Vec::new();
    for path in list_sessions(base, project)? {
        let state = ReviewState::load(&path)?;
        let live = state.comments.iter().filter(|c| !c.deleted);
        let comments = live.clone().count();
        let open = live.filter(|c| !c.resolved).count();
        rows.push(SessionRow {
            ulid: state.session.ulid,
            source: state.session.source,
            active: active.as_deref() == Some(path.as_path()),
            comments,
            open,
        });
    }
    Ok(rows)
}

/// Render the session groups. With `show_projects`, each group leads with its
/// project name and its rows are indented beneath it; otherwise the single
/// group's rows are printed flat.
fn render_list(groups: &[(String, Vec<SessionRow>)], show_projects: bool) -> String {
    if groups.is_empty() {
        return "No sessions.\n".to_string();
    }
    let mut out = String::new();
    for (project, rows) in groups {
        let indent = if show_projects {
            out.push_str(&format!("{project}\n"));
            "  "
        } else {
            ""
        };
        for row in rows {
            let marker = if row.active { '*' } else { ' ' };
            out.push_str(&format!(
                "{indent}{marker} {}  {}  {} comment{}, {} open\n",
                row.ulid,
                row.source.as_str(),
                row.comments,
                if row.comments == 1 { "" } else { "s" },
                row.open,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{SessionRow, render_list};
    use ulid::Ulid;
    use wiff_core::record::SourceKind;

    fn ulid(text: &str) -> Ulid {
        Ulid::from_string(text).unwrap()
    }

    #[test]
    fn lists_a_single_project_flat_with_the_active_session_marked() {
        let groups = vec![(
            "demo".to_string(),
            vec![
                SessionRow {
                    ulid: ulid("00000000000000000000000001"),
                    source: SourceKind::GitWorktree,
                    active: true,
                    comments: 3,
                    open: 1,
                },
                SessionRow {
                    ulid: ulid("00000000000000000000000002"),
                    source: SourceKind::Stdin,
                    active: false,
                    comments: 0,
                    open: 0,
                },
            ],
        )];
        let out = render_list(&groups, false);
        let expected = "\
* 00000000000000000000000001  git_worktree  3 comments, 1 open
  00000000000000000000000002  stdin  0 comments, 0 open
";
        k9::assert_equal!(out, expected.to_string());
    }

    #[test]
    fn lists_every_project_under_its_name() {
        let groups = vec![
            (
                "demo".to_string(),
                vec![SessionRow {
                    ulid: ulid("00000000000000000000000001"),
                    source: SourceKind::GitWorktree,
                    active: true,
                    comments: 1,
                    open: 0,
                }],
            ),
            (
                "other".to_string(),
                vec![SessionRow {
                    ulid: ulid("00000000000000000000000002"),
                    source: SourceKind::Stdin,
                    active: false,
                    comments: 2,
                    open: 2,
                }],
            ),
        ];
        let out = render_list(&groups, true);
        let expected = "\
demo
  * 00000000000000000000000001  git_worktree  1 comment, 0 open
other
    00000000000000000000000002  stdin  2 comments, 2 open
";
        k9::assert_equal!(out, expected.to_string());
    }

    #[test]
    fn reports_when_there_are_no_sessions() {
        let out = render_list(&[], true);
        k9::assert_equal!(out, "No sessions.\n".to_string());
    }
}
