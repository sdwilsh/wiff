//! `wiff session`: list and remove review sessions.

use anyhow::Context;
use clap::{Args, Subcommand};
use time::OffsetDateTime;
use wiff_core::record::{ForgeUrl, RevisionId, ScmSource, SourceKind, TipRule};
use wiff_core::review::ReviewState;
use wiff_core::session::{
    active_session, data_dir, id_from_path, list_projects, list_sessions, remove_session,
    resolve_session_id, session_file,
};
use wiff_core::{ProjectIdentity, ScmType, SessionId};

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
        let (projects, identity) = if self.all {
            (list_projects(&base)?, None)
        } else {
            let cwd =
                std::env::current_dir().context("could not determine the current directory")?;
            let identity = ProjectIdentity::for_dir_or_forced(&cwd, self.project.as_deref())?;
            (vec![identity.canonical.clone()], Some(identity))
        };
        let mut groups = Vec::new();
        for project in projects {
            // The active marker follows the same branch-aware discovery the
            // acting commands use, but only for the checked-out repository; a
            // cross-project listing has no single repo context.
            let (root, scm) = match &identity {
                Some(id) if id.canonical == project => (id.repo_root.as_deref(), id.scm),
                _ => (None, None),
            };
            let rows = session_rows(&base, &project, root, scm)?;
            if !rows.is_empty() {
                groups.push((project, rows));
            }
        }
        print!(
            "{}",
            render_list(&groups, self.all, wiff_core::determinism::now())
        );
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
        let id = resolve_session_id(&base, &identity.canonical, &self.id)?;
        let path = session_file(&base, &identity.canonical, id);
        remove_session(&path)?;
        println!("removed session {id}");
        Ok(())
    }
}

/// A session in the listing, either readable or not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionEntry {
    /// A session whose log folded cleanly into a summary.
    Summary(SessionRow),
    /// A session whose log could not be read, labelled by its id (or file
    /// name when that cannot be decoded) alongside the reason.
    Unreadable {
        /// The session's id as text, or its file name when the name is not a
        /// valid id.
        label: String,
        /// The human-readable reason the log could not be loaded.
        reason: String,
    },
}

/// A summary of a session for the two-line listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRow {
    /// The session's identity.
    id: SessionId,
    /// How its diff was captured.
    source: SourceKind,
    /// The pull request the session mirrors, when it has forge linkage.
    forge: Option<ForgeUrl>,
    /// The base commit of the latest captured version, when the source has an
    /// authoritative base.
    base_revision: Option<RevisionId>,
    /// The tip commit of the latest captured version. Absent for a working-copy
    /// or index capture, whose tip is the uncommitted state.
    head_revision: Option<RevisionId>,
    /// When the session was created, decoded from its id.
    created_at: OffsetDateTime,
    /// When the session log was last written, from its file mtime.
    updated_at: OffsetDateTime,
    /// Whether it is the project's active (most recent) session.
    active: bool,
    /// The number of live (non-withdrawn) comments.
    comments: usize,
    /// How many of those are still open (unresolved).
    open: usize,
    /// The number of files in the latest captured version, shown for an explore
    /// session whose reviewed set is that file list.
    files: usize,
}

/// Summarize a project's sessions, most recent first, marking the active one.
///
/// A session whose log cannot be folded (an incompatible format version, or a
/// corrupt file) does not abort the listing: it becomes an `Unreadable` entry
/// noting the reason, and the remaining sessions still list.
fn session_rows(
    base: &std::path::Path,
    project: &str,
    repo_root: Option<&std::path::Path>,
    scm: Option<ScmType>,
) -> anyhow::Result<Vec<SessionEntry>> {
    let active = active_session(base, project, repo_root, scm).ok();
    let mut rows = Vec::new();
    for path in list_sessions(base, project)? {
        let state = match ReviewState::load(&path) {
            Ok(state) => state,
            // A session removed between enumeration and this read simply drops
            // out of the listing; only a genuine format or content problem
            // becomes an entry the reviewer can see.
            Err(err) if vanished(&err) => continue,
            Err(err) => {
                rows.push(SessionEntry::Unreadable {
                    label: session_label(&path),
                    reason: err.to_string(),
                });
                continue;
            }
        };
        let modified = match std::fs::metadata(&path).and_then(|meta| meta.modified()) {
            Ok(modified) => modified,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(source)
                    .with_context(|| format!("reading the mtime of {}", path.display()));
            }
        };
        let live = state.comments.iter().filter(|c| !c.deleted);
        let comments = live.clone().count();
        let open = live.filter(|c| !c.resolved).count();
        let base_revision = state.latest_version().and_then(|v| v.base_revision.clone());
        let head_revision = state.latest_version().and_then(|v| v.head_revision.clone());
        let files = state.latest_version().map(|v| v.files.len()).unwrap_or(0);
        rows.push(SessionEntry::Summary(SessionRow {
            id: state.session.id,
            source: state.session.source,
            forge: state.session.forge,
            base_revision,
            head_revision,
            created_at: state.session.id.created_at(),
            updated_at: OffsetDateTime::from(modified),
            active: active.as_deref() == Some(path.as_path()),
            comments,
            open,
            files,
        }));
    }
    Ok(rows)
}

/// Whether a load failure means the session file is gone, having been removed
/// between the directory listing and the read.
fn vanished(err: &wiff_core::Error) -> bool {
    matches!(
        err,
        wiff_core::Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound
    )
}

/// The label for an unreadable session: its id, or its file name when the name
/// is not a valid id.
fn session_label(path: &std::path::Path) -> String {
    id_from_path(path)
        .map(|id| id.to_string())
        .unwrap_or_else(|_| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string()
        })
}

/// Render the session groups. With `show_projects`, each group leads with its
/// project name and its rows are indented beneath it; otherwise the single
/// group's rows are printed flat. `now` anchors the relative creation and
/// modification times.
fn render_list(
    groups: &[(String, Vec<SessionEntry>)],
    show_projects: bool,
    now: OffsetDateTime,
) -> String {
    if groups.is_empty() {
        return "No sessions.\n".to_string();
    }
    let mut out = String::new();
    for (project, entries) in groups {
        let indent = if show_projects {
            out.push_str(&format!("{project}\n"));
            "  "
        } else {
            ""
        };
        for entry in entries {
            match entry {
                SessionEntry::Summary(row) => {
                    let marker = if row.active { '*' } else { ' ' };
                    out.push_str(&format!(
                        "{indent}{marker} {}  {}  {}\n",
                        row.id,
                        headline(row),
                        tally(row.comments, row.open),
                    ));
                    out.push_str(&format!("{indent}    {}\n", detail(row, now)));
                }
                SessionEntry::Unreadable { label, reason } => {
                    out.push_str(&format!("{indent}  {label}  unreadable session\n"));
                    out.push_str(&format!("{indent}    {reason}\n"));
                }
            }
        }
    }
    out
}

/// Returns the label that identifies a session on the first line of its row.
fn headline(row: &SessionRow) -> String {
    if let Some(url) = &row.forge {
        return url.to_string();
    }
    match &row.source {
        SourceKind::Scm(scm) => scm_headline(scm),
        SourceKind::Stdin => "stdin snapshot".to_string(),
        SourceKind::Forge => "forge".to_string(),
        SourceKind::Explore => "exploring code".to_string(),
        SourceKind::Unknown => "unknown source".to_string(),
    }
}

/// Returns the identifying label for an scm source that has no forge URL.
fn scm_headline(scm: &ScmSource) -> String {
    if let Some(branch) = branch_name(scm) {
        return branch;
    }
    match &scm.tip {
        TipRule::ChangeId { id } => id.as_str().to_string(),
        TipRule::Pinned { revision } => format!("revision {}", short(revision)),
        TipRule::WorkingCopy | TipRule::Index | TipRule::Ref { .. } => "(detached)".to_string(),
    }
}

/// Returns the branch a source sits on, or `None` when it records none.
fn branch_name(scm: &ScmSource) -> Option<String> {
    if let Some(hint) = &scm.branch_hint {
        return Some(strip_ref(hint));
    }
    match &scm.tip {
        TipRule::Ref { name } => Some(strip_ref(name)),
        _ => None,
    }
}

/// Returns the second line of a row: the captured range and timings.
///
/// A forge capture has a concrete `base..head` range but cannot yet be
/// recaptured, so its range shows alongside a note to that effect; a stdin diff
/// has no range at all.
fn detail(row: &SessionRow, now: OffsetDateTime) -> String {
    let times = times_clause(row.created_at, row.updated_at, now);
    match &row.source {
        SourceKind::Scm(scm) => {
            let range = source_range(&row.base_revision, &row.head_revision, uncommitted_tip(scm));
            format!("{range}  base {}  {times}", scm.base.as_str())
        }
        SourceKind::Forge => {
            let range = source_range(&row.base_revision, &row.head_revision, "?");
            format!("{range}  not regenerable  {times}")
        }
        SourceKind::Stdin => format!("one-shot diff, not regenerable  {times}"),
        SourceKind::Explore => {
            let plural = if row.files == 1 { "" } else { "s" };
            format!("{} file{plural} under review  {times}", row.files)
        }
        SourceKind::Unknown => format!("unrecognized source, not regenerable  {times}"),
    }
}

/// Returns the captured range as short `base..tip` hashes, showing `uncommitted`
/// as the tip when the capture pinned no head commit.
fn source_range(base: &Option<RevisionId>, head: &Option<RevisionId>, uncommitted: &str) -> String {
    let base = base.as_ref().map_or_else(|| "?".to_string(), short);
    let tip = head.as_ref().map_or_else(|| uncommitted.to_string(), short);
    format!("{base}..{tip}")
}

/// Returns the word for an scm tip that has no head commit of its own, used in
/// place of a hash for a working-copy or index capture.
fn uncommitted_tip(scm: &ScmSource) -> &'static str {
    match scm.tip {
        TipRule::WorkingCopy => "working copy",
        TipRule::Index => "index",
        _ => "?",
    }
}

/// The comment tally: the live count and how many stay open, or a note when
/// there are none.
fn tally(comments: usize, open: usize) -> String {
    if comments == 0 {
        return "(no comments)".to_string();
    }
    let plural = if comments == 1 { "" } else { "s" };
    format!("({comments} comment{plural}, {open} open)")
}

/// Returns the creation and modification times as relative ages. The
/// modification age is dropped for a session never written since it was
/// captured, its log's mtime still within a second of its creation.
fn times_clause(created: OffsetDateTime, updated: OffsetDateTime, now: OffsetDateTime) -> String {
    let created_age = relative(created, now);
    if (updated - created).whole_seconds() <= 0 {
        format!("created {created_age}")
    } else {
        format!("created {created_age}, updated {}", relative(updated, now))
    }
}

/// A coarse age of `from` relative to `now`, in the largest whole unit up to
/// weeks. A future time (from a clock skew) reads as the present.
fn relative(from: OffsetDateTime, now: OffsetDateTime) -> String {
    let secs = (now - from).whole_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else if secs < 604_800 {
        format!("{}d ago", secs / 86_400)
    } else {
        format!("{}w ago", secs / 604_800)
    }
}

/// The first seven characters of a revision, the customary short hash.
fn short(revision: &RevisionId) -> String {
    revision.as_str().chars().take(7).collect()
}

/// Strip a `refs/heads/` prefix, leaving other ref names (and short branch
/// names) untouched.
fn strip_ref(name: &str) -> String {
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

#[cfg(test)]
mod tests {
    use super::{SessionEntry, SessionRow, render_list};
    use time::OffsetDateTime;

    use wiff_core::record::{ChangeId, ForgeUrl, RevisionId, ScmSource, SourceKind, TipRule};
    use wiff_core::{BaseRuleset, ScmType, SessionId};

    fn session_id(text: &str) -> SessionId {
        text.parse().unwrap()
    }

    /// An inactive row created and last written an hour ago, for tests that
    /// exercise the headline and range rather than the timings.
    fn row(
        id_text: &str,
        source: SourceKind,
        forge: Option<ForgeUrl>,
        base: Option<RevisionId>,
        head: Option<RevisionId>,
        comments: usize,
        open: usize,
    ) -> SessionEntry {
        SessionEntry::Summary(SessionRow {
            id: session_id(id_text),
            source,
            forge,
            base_revision: base,
            head_revision: head,
            created_at: ago(3_600),
            updated_at: ago(3_600),
            active: false,
            comments,
            open,
            files: 0,
        })
    }

    /// A fixed reference point for the relative times.
    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap()
    }

    /// `now` less `secs` seconds, for placing a row's created or modified time.
    fn ago(secs: i64) -> OffsetDateTime {
        now() - time::Duration::seconds(secs)
    }

    fn scm(base: &str, tip: TipRule, branch_hint: Option<&str>) -> SourceKind {
        SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new(base),
            tip,
            branch_hint: branch_hint.map(str::to_string),
        })
    }

    fn rev(text: &str) -> RevisionId {
        RevisionId(text.to_string())
    }

    #[test]
    fn lists_each_source_kind_with_its_range_ruleset_and_ages() {
        let rows = vec![
            // A plain `wiff new`: the working copy on a branch, base pinned
            // at the commit it was created on.
            SessionRow {
                id: session_id("000000001"),
                source: scm(
                    "ref(name(deadbeef))",
                    TipRule::WorkingCopy,
                    Some("refs/heads/topic"),
                ),
                forge: None,
                base_revision: Some(rev("a1b2c3d0000")),
                head_revision: None,
                created_at: ago(7_200),
                updated_at: ago(720),
                active: true,
                comments: 3,
                open: 1,
                files: 0,
            },
            // `wiff new --from-base`: the same working copy, its base taken
            // from the configured whole-branch ruleset instead.
            SessionRow {
                id: session_id("000000002"),
                source: scm(
                    "merge-base(trunk)",
                    TipRule::WorkingCopy,
                    Some("refs/heads/topic"),
                ),
                forge: None,
                base_revision: Some(rev("e4f5a6b0000")),
                head_revision: None,
                created_at: ago(86_400),
                updated_at: ago(86_400),
                active: false,
                comments: 0,
                open: 0,
                files: 0,
            },
            // `wiff forge fetch`: a pinned range with both endpoints, and
            // the pull request URL as its identity.
            SessionRow {
                id: session_id("000000003"),
                source: scm(
                    "merge-base(name(9f8e7d6))",
                    TipRule::Pinned {
                        revision: rev("d4c3b2a0000"),
                    },
                    None,
                ),
                forge: Some(ForgeUrl::parse("https://github.com/o/r/pull/42").unwrap()),
                base_revision: Some(rev("9f8e7d60000")),
                head_revision: Some(rev("d4c3b2a0000")),
                created_at: ago(259_200),
                updated_at: ago(14_400),
                active: false,
                comments: 5,
                open: 2,
                files: 0,
            },
            // A working copy captured on a detached head names no branch.
            SessionRow {
                id: session_id("000000004"),
                source: scm("ref(name(deadbeef))", TipRule::WorkingCopy, None),
                forge: None,
                base_revision: Some(rev("aabbccd0000")),
                head_revision: None,
                created_at: ago(432_000),
                updated_at: ago(432_000),
                active: false,
                comments: 0,
                open: 0,
                files: 0,
            },
            // A diff piped in on stdin: a one-shot snapshot.
            SessionRow {
                id: session_id("000000005"),
                source: SourceKind::Stdin,
                forge: None,
                base_revision: None,
                head_revision: None,
                created_at: ago(1_209_600),
                updated_at: ago(1_209_600),
                active: false,
                comments: 1,
                open: 0,
                files: 0,
            },
        ];
        let groups = vec![(
            "demo".to_string(),
            rows.into_iter().map(SessionEntry::Summary).collect(),
        )];
        let out = render_list(&groups, false, now());
        let expected = "\
* 000000001  topic  (3 comments, 1 open)
    a1b2c3d..working copy  base ref(name(deadbeef))  created 2h ago, updated 12m ago
  000000002  topic  (no comments)
    e4f5a6b..working copy  base merge-base(trunk)  created 1d ago
  000000003  https://github.com/o/r/pull/42  (5 comments, 2 open)
    9f8e7d6..d4c3b2a  base merge-base(name(9f8e7d6))  created 3d ago, updated 4h ago
  000000004  (detached)  (no comments)
    aabbccd..working copy  base ref(name(deadbeef))  created 5d ago
  000000005  stdin snapshot  (1 comment, 0 open)
    one-shot diff, not regenerable  created 2w ago
";
        wince::assert_eq!(out, expected.to_string());
    }

    #[test]
    fn lists_every_project_under_its_name() {
        let groups = vec![
            (
                "demo".to_string(),
                vec![SessionEntry::Summary(SessionRow {
                    id: session_id("000000001"),
                    source: scm(
                        "ref(name(deadbeef))",
                        TipRule::WorkingCopy,
                        Some("refs/heads/topic"),
                    ),
                    forge: None,
                    base_revision: Some(rev("a1b2c3d0000")),
                    head_revision: None,
                    created_at: ago(7_200),
                    updated_at: ago(7_200),
                    active: true,
                    comments: 1,
                    open: 0,
                    files: 0,
                })],
            ),
            (
                "other".to_string(),
                vec![SessionEntry::Summary(SessionRow {
                    id: session_id("000000002"),
                    source: SourceKind::Stdin,
                    forge: None,
                    base_revision: None,
                    head_revision: None,
                    created_at: ago(86_400),
                    updated_at: ago(86_400),
                    active: false,
                    comments: 2,
                    open: 2,
                    files: 0,
                })],
            ),
        ];
        let out = render_list(&groups, true, now());
        let expected = "\
demo
  * 000000001  topic  (1 comment, 0 open)
      a1b2c3d..working copy  base ref(name(deadbeef))  created 2h ago
other
    000000002  stdin snapshot  (2 comments, 2 open)
      one-shot diff, not regenerable  created 1d ago
";
        wince::assert_eq!(out, expected.to_string());
    }

    #[test]
    fn a_forge_import_shows_its_range_and_that_it_cannot_be_regenerated() {
        let groups = vec![(
            "demo".to_string(),
            vec![row(
                "000000001",
                SourceKind::Forge,
                Some(ForgeUrl::parse("https://github.com/o/r/pull/7").unwrap()),
                Some(rev("9f8e7d60000")),
                Some(rev("d4c3b2a0000")),
                2,
                0,
            )],
        )];
        let out = render_list(&groups, false, now());
        let expected = "  000000001  https://github.com/o/r/pull/7  (2 comments, 0 open)
    9f8e7d6..d4c3b2a  not regenerable  created 1h ago
";
        wince::assert_eq!(out, expected.to_string());
    }

    #[test]
    fn a_headline_names_the_branch_change_or_pinned_revision() {
        let groups = vec![(
            "demo".to_string(),
            vec![
                // `wiff new --change main`: a ref tip resolves to a commit and
                // reads as its branch.
                row(
                    "000000001",
                    scm(
                        "parent(@)",
                        TipRule::Ref {
                            name: "refs/heads/main".to_string(),
                        },
                        None,
                    ),
                    None,
                    Some(rev("1111111000")),
                    Some(rev("2222222000")),
                    0,
                    0,
                ),
                // The staged index on a branch reads as that branch.
                row(
                    "000000002",
                    scm("ref(name(x))", TipRule::Index, Some("refs/heads/dev")),
                    None,
                    Some(rev("3333333000")),
                    None,
                    0,
                    0,
                ),
                // A change tip reads as the change it names.
                row(
                    "000000003",
                    scm(
                        "parent(@)",
                        TipRule::ChangeId {
                            id: ChangeId("zxcvbnm".to_string()),
                        },
                        None,
                    ),
                    None,
                    Some(rev("4444444000")),
                    Some(rev("5555555000")),
                    0,
                    0,
                ),
                // A detached index names no branch.
                row(
                    "000000004",
                    scm("ref(name(x))", TipRule::Index, None),
                    None,
                    Some(rev("6666666000")),
                    None,
                    0,
                    0,
                ),
            ],
        )];
        let out = render_list(&groups, false, now());
        let expected = "  000000001  main  (no comments)
    1111111..2222222  base parent(@)  created 1h ago
  000000002  dev  (no comments)
    3333333..index  base ref(name(x))  created 1h ago
  000000003  zxcvbnm  (no comments)
    4444444..5555555  base parent(@)  created 1h ago
  000000004  (detached)  (no comments)
    6666666..index  base ref(name(x))  created 1h ago
";
        wince::assert_eq!(out, expected.to_string());
    }

    #[test]
    fn an_unreadable_session_lists_alongside_the_readable_ones() {
        let groups = vec![(
            "demo".to_string(),
            vec![
                SessionEntry::Unreadable {
                    label: "000000009".to_string(),
                    reason: "session format version 4 does not match supported version 6"
                        .to_string(),
                },
                row("000000001", SourceKind::Stdin, None, None, None, 0, 0),
            ],
        )];
        let out = render_list(&groups, false, now());
        let expected = "  000000009  unreadable session
    session format version 4 does not match supported version 6
  000000001  stdin snapshot  (no comments)
    one-shot diff, not regenerable  created 1h ago
";
        wince::assert_eq!(out, expected.to_string());
    }

    #[test]
    fn reports_when_there_are_no_sessions() {
        let out = render_list(&[], true, now());
        wince::assert_eq!(out, "No sessions.\n".to_string());
    }
}
