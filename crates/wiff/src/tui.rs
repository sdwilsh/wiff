//! Opening a session in the review TUI.
//!
//! This is the bridge from the persisted session to the interactive review: it
//! loads the latest captured diff, renders it, and runs the terminal loop, then
//! keeps or removes the session according to how the reviewer chose to leave.

use std::path::Path;

use anyhow::Context;
use wiff_config::{Config, OnExit};
use wiff_core::record::{
    Author, AuthorKind, CommentEventKind, RecordBody, SessionHeader, VersionNumber,
};
use wiff_core::session::{SessionWatcher, read_records, remove_session};
use wiff_core::{
    AnchorFailures, CapturedDiff, LockWait, RefreshOutcome, ReviewState, SessionLog, SidebandHash,
    capture_draft_anchors, compare_versions, refresh_session,
};
use wiff_tui::{
    App, CommentSync, CompareRequest, DiffView, Exit, ExitDefault, KeyHints, Review, Theme, run,
};

use crate::command::recapture_diff;

/// Parse unified diff `text` and expand its tabs to spaces at `tab_width` column
/// stops. Raw tabs are never rendered, since they would break the review's
/// fixed-column alignment.
fn parse_diff(text: &str, tab_width: usize) -> Result<wiff_diff::Diff, wiff_diff::ParseError> {
    let mut diff = wiff_diff::parse(text)?;
    diff.expand_tabs(tab_width);
    Ok(diff)
}

/// Open `session_path` in the review TUI, then keep or remove the session per
/// the reviewer's choice and the configured `on_exit` default. When
/// `offer_refresh` is set and recapturing the source would produce a diff
/// different from the latest captured version, a modal offers to refresh once
/// the existing state is on screen.
pub fn open(session_path: &Path, config: &Config, offer_refresh: bool) -> anyhow::Result<()> {
    let log = SessionLog::open(session_path)?;
    let state = ReviewState::load(session_path)?;
    let version = state
        .latest_version()
        .context("this session has no captured diff to review")?;
    let text = log.read_diff(version.number)?;
    let diff = parse_diff(&text, config.tab_width)?;

    let theme = Theme::dark();
    let sections = wiff_diff::SectionMatchers::new(&config.section)
        .context("a configured section pattern is not a valid regex")?;
    // Withdrawn comments are tombstones in the folded state; the review view
    // shows only the live ones.
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    let keymap = config.keymap()?;
    let view = DiffView::new(theme.clone())?
        .with_display_context(config.display_context)
        .with_section_matchers(sections)
        .with_key_hints(KeyHints::from_keymap(&keymap));
    // Comments authored in the TUI are attributed to the human reviewer and
    // anchored against the diff version being reviewed.
    let author = config.author.resolve(AuthorKind::Human);
    let review = Review::deferred(
        view,
        diff,
        author.clone(),
        version.number.get(),
        comments,
        state.description.clone(),
    );
    let mut app = App::reviewing(review, 0, &theme)
        .with_exit_default(exit_default(config.on_exit))
        .with_keymap(keymap.clone())
        .with_wrap_content(config.wrap_lines)
        .with_diff_mode(config.diff_mode, config.side_by_side_min_width)
        .with_tab_width(config.tab_width)
        .with_nudge_to_detach(config.nudge_to_detach);
    // A resumed session whose source has moved on opens over the existing state
    // with a prompt to recapture it, rather than silently showing a stale diff.
    if offer_refresh && source_changed(&state) {
        app.offer_refresh();
    }

    // Refresh recaptures the diff and reloads the app in place; save commits the
    // pending drafts and keeps the review open. Any failure is reported in the
    // status line rather than tearing down the review.
    let refresh = |app: &mut App| {
        if let Err(err) = refresh_in_place(session_path, &author, config.tab_width, app) {
            app.set_message(format!("refresh failed: {err}"));
        }
    };
    let save = |app: &mut App| {
        if let Err(err) = save_in_place(session_path, app) {
            app.set_message(format!("save failed: {err}"));
        }
    };
    let compare = |app: &mut App, request: CompareRequest| {
        if let Err(err) = compare_in_place(session_path, config.tab_width, app, request) {
            app.set_message(format!("compare failed: {err}"));
        }
    };
    // Between key presses, pick up comments another actor (an agent, or a second
    // human) has committed to this session and fold them in. The watcher is a
    // cheap stat, so it costs nothing while the file is untouched.
    let mut watcher = SessionWatcher::new(session_path);
    let sync = |app: &mut App| {
        let Some(fingerprint) = watcher.changed() else {
            return false;
        };
        // Act only on a successful read. A read that fails because a line is
        // still being appended leaves the change unacknowledged, so the next
        // wakeup retries it once the line is whole.
        let Ok(summary) = reload_committed(session_path, app) else {
            return false;
        };
        watcher.acknowledge(fingerprint);
        if summary.is_empty() {
            return false;
        }
        app.set_message(sync_report(&summary));
        true
    };

    let (exit, drafts) = run(app, keymap, refresh, save, sync, compare)?;
    resolve_exit(exit, session_path, drafts)
}

/// Reconstruct the diff the reviewer chose to compare against and show it in
/// place: an earlier version's after content on the left against the latest
/// version's, or the latest version's own captured diff when returning to it.
/// Reports what is shown in the status line.
fn compare_in_place(
    session_path: &Path,
    tab_width: usize,
    app: &mut App,
    request: CompareRequest,
) -> anyhow::Result<()> {
    let state = ReviewState::load(session_path)?;
    let latest = state
        .latest_version()
        .context("this session has no captured diff to compare")?
        .number;
    let log = SessionLog::open(session_path)?;
    let latest_diff = parse_diff(&log.read_diff(latest)?, tab_width)?;
    match request {
        CompareRequest::Latest => {
            app.show_comparison(latest_diff, None);
            app.set_message(format!("showing the latest diff (v{latest})"));
        }
        CompareRequest::Version(from) => {
            let from_diff = parse_diff(&log.read_diff(VersionNumber(from))?, tab_width)?;
            let comparison = compare_versions(&latest_diff, latest.get(), &from_diff, from);
            app.show_comparison(comparison.diff, Some((from, comparison.before_origin)));
            app.set_message(format!("comparing v{from} against v{latest}"));
        }
    }
    Ok(())
}

/// Recapture the session's diff as a new version, rebase its committed comments
/// and the reviewer's pending drafts onto it, and reload `app` over the full new
/// diff, reporting the tally in the status line and offering the
/// version-comparison list to narrow the view, opened on where the reviewer last
/// left comments. A no-op capture (nothing changed) says so instead.
fn refresh_in_place(
    session_path: &Path,
    author: &Author,
    tab_width: usize,
    app: &mut App,
) -> anyhow::Result<()> {
    // The reference version the reviewer was comparing against before the
    // recapture, or none when they were on the latest diff. The recapture resets
    // the view, but the picker below marks this as where they were.
    let viewing = app.comparing_from();
    let state = ReviewState::load(session_path)?;
    let prior_latest = state.latest_version().map(|v| v.number.get());
    let captured = recapture(&state.session)?;
    let mut log = SessionLog::open(session_path)?;
    let outcome = match refresh_session(&mut log, &captured, author.clone(), LockWait::NonBlock)? {
        Some(outcome) => outcome,
        None => {
            let current = prior_latest.unwrap_or(0);
            app.set_message(format!("no changes since v{current}"));
            return Ok(());
        }
    };
    // The newest version at or before the pre-refresh latest where the reviewer
    // left comments is where their in-progress work sits; the picker offers
    // comparing against it.
    let last_commented = match prior_latest {
        Some(viewed) => last_commented_version(session_path, author, viewed)?,
        None => None,
    };

    let state = ReviewState::load(session_path)?;
    let latest = state
        .latest_version()
        .context("the refreshed session has no captured diff")?
        .number;
    let latest_diff = parse_diff(&log.read_diff(latest)?, tab_width)?;
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    app.refresh(latest_diff, comments, latest.get(), |authored_version| {
        parse_diff(&log.read_diff(VersionNumber(authored_version))?, tab_width).map_err(Into::into)
    })?;
    app.set_message(refresh_report(&outcome));
    // Offer the version list from the reviewer's pre-refresh perspective: their
    // prior view is marked and pre-selected, and cancelling keeps it against the
    // fresh capture, rather than switching under them mid-review.
    app.offer_compare_after_refresh(viewing, last_commented);
    Ok(())
}

/// The newest diff version at or before `viewed` that `author` committed a
/// comment against, read from the session's records. Reads the authored version
/// from each comment record, which is fixed at commit time even as later
/// refreshes rebase the comment's anchor forward. None when they committed no
/// comment at or before that version.
fn last_commented_version(
    session_path: &Path,
    author: &Author,
    viewed: u32,
) -> anyhow::Result<Option<u32>> {
    Ok(read_records(session_path)?
        .into_iter()
        .filter_map(|record| match record.body {
            RecordBody::CommentEvent(event) if event.author == *author => match event.kind {
                CommentEventKind::Create(create) => Some(create.version.get()),
                _ => None,
            },
            _ => None,
        })
        .filter(|version| *version <= viewed)
        .max())
}

/// Commit the reviewer's pending drafts to the session log and reload the
/// review's committed comments over them, keeping the review open. Reports the
/// tally in the status line; a no-op when nothing is pending.
fn save_in_place(session_path: &Path, app: &mut App) -> anyhow::Result<()> {
    // Keep the drafts in the buffer until the commit is durable. A contended or
    // diverged log, or a torn read of the position check, fails the commit; the
    // buffer is then untouched and the reviewer can retry rather than lose work.
    let drafts = app.draft_records();
    if drafts.is_empty() {
        app.set_message("nothing to save".to_string());
        return Ok(());
    }
    let count = drafts.len();
    let anchor_failures = commit_drafts(session_path, drafts)?;
    app.clear_drafts();
    let changes = format!("{count} change{}", if count == 1 { "" } else { "s" });
    // The commit is durable once it returns; a reload failure here only leaves
    // the view stale until the next sync tick, so report the commit as done
    // rather than as a failed save that discarded nothing.
    let mut message = match reload_committed(session_path, app) {
        Ok(_) => format!("committed {changes}"),
        Err(err) => format!("committed {changes}; view refresh failed: {err}"),
    };
    // Report unanchored comments as a count: the status line is one row, and the
    // per-fault detail would bury the commit result. A comment still commits,
    // just without rebasing support. The causes are dropped here; a reviewer who
    // wants them sees the per-cause detail on the exit-commit path instead.
    if let Some(summary) = anchor_failures.summary() {
        message.push_str("; ");
        message.push_str(&summary);
    }
    app.set_message(message);
    Ok(())
}

/// Reload the review's committed comments from the session log, dropping the
/// withdrawn ones, and report how they differ from what the app was showing.
fn reload_committed(session_path: &Path, app: &mut App) -> anyhow::Result<CommentSync> {
    let state = ReviewState::load(session_path)?;
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    Ok(app.reload_comments(comments, state.description.clone()))
}

/// A terse status note naming what another actor changed, joining only the parts
/// that are non-zero.
fn sync_report(summary: &CommentSync) -> String {
    let mut parts = Vec::new();
    if summary.added > 0 {
        parts.push(format!("{} added", summary.added));
    }
    if summary.changed > 0 {
        parts.push(format!("{} updated", summary.changed));
    }
    if summary.removed > 0 {
        parts.push(format!("{} removed", summary.removed));
    }
    if summary.description_changed {
        parts.push("description updated".to_string());
    }
    format!("synced: {}", parts.join(", "))
}

/// Whether recapturing the session's source would produce a diff different from
/// its latest captured version. False when the source cannot be recaptured (a
/// stdin diff) or the recapture fails, so opening a session is never blocked on
/// it.
fn source_changed(state: &ReviewState) -> bool {
    let Some(latest) = state.latest_version() else {
        return false;
    };
    let recaptured = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(recapture_diff(&state.session))
    });
    matches!(recaptured, Ok(Some(captured)) if SidebandHash::of(captured.text.as_bytes()) != latest.diff_hash)
}

/// Recapture the diff from the session's original source. A stdin source cannot
/// be reread inside the TUI, since stdin is now the terminal, so it is directed
/// to the `wiff refresh` command instead.
fn recapture(header: &SessionHeader) -> anyhow::Result<CapturedDiff> {
    // The event loop runs on a tokio worker, so block on the async recapture
    // without standing up a nested runtime.
    let captured = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(recapture_diff(header))
    })?;
    captured.context(
        "this session's diff came from stdin; refresh it with `wiff refresh` and a new piped diff",
    )
}

/// The status-line tally of a refresh: the captured version and how its comments
/// fared.
fn refresh_report(outcome: &RefreshOutcome) -> String {
    let total = outcome.exact + outcome.approximate + outcome.relocated + outcome.outdated;
    format!(
        "captured v{}; rebased {total} comment{}: {} exact, {} shifted, {} moved, {} outdated",
        outcome.version,
        if total == 1 { "" } else { "s" },
        outcome.exact,
        outcome.approximate,
        outcome.relocated,
        outcome.outdated,
    )
}

/// The TUI's exit default matching the configured `on_exit` policy.
fn exit_default(on_exit: OnExit) -> ExitDefault {
    match on_exit {
        OnExit::Keep => ExitDefault::Keep,
        OnExit::Remove => ExitDefault::Remove,
        OnExit::Prompt => ExitDefault::Prompt,
    }
}

/// Carry out the reviewer's chosen `exit`: commit the buffered drafts and keep
/// the session, keep it and drop the drafts, or remove it entirely.
fn resolve_exit(exit: Exit, session_path: &Path, drafts: Vec<RecordBody>) -> anyhow::Result<()> {
    match exit {
        Exit::Commit => {
            // Report the same count the interactive save shows, then the causes
            // stderr has room for that the one-row status line does not.
            let failures = commit_drafts(session_path, drafts)?;
            if let Some(summary) = failures.summary() {
                eprintln!("warning: {summary}");
                for cause in &failures.errors {
                    eprintln!("  {cause}");
                }
            }
            println!("kept session at {}", session_path.display());
        }
        Exit::Discard => {
            println!(
                "kept session at {} (drafts discarded)",
                session_path.display()
            );
        }
        Exit::Remove => {
            remove_session(session_path)?;
            println!("removed session");
        }
    }
    Ok(())
}

/// Commit the reviewer's buffered drafts, capturing each line comment's anchor
/// first. Returns the comments left unanchored by a damaged session and the
/// distinct faults behind them; those comments still commit, as bare locators.
fn commit_drafts(
    session_path: &Path,
    mut drafts: Vec<RecordBody>,
) -> anyhow::Result<AnchorFailures> {
    if drafts.is_empty() {
        return Ok(AnchorFailures::default());
    }
    let mut log = SessionLog::open(session_path)?;
    // Capture anchors before the append: the anchor must be part of the same
    // committed batch as the comment it belongs to. This reads each version's
    // sideband diff without the session lock; a written version's diff is fixed
    // once its record is appended, and if a concurrent removal takes it out from
    // under this read the comment simply commits as a bare locator.
    let anchor_failures = capture_draft_anchors(&log, &mut drafts);
    // Buffered drafts append as a batch that rejects a diverged file rather
    // than resyncing to it. The reviewer composed them against the view folded
    // at save time; failing the save on a concurrent write lets the view reload
    // and reconcile before the reviewer retries.
    log.append_all_locked(drafts)?;
    Ok(anchor_failures)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use ulid::Ulid;
    use wiff_core::comment::{delete_event, resolve_event};
    use wiff_core::record::{
        Author, AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentTarget,
        RecordBody, SessionHeader, SourceKind, VersionNumber,
    };
    use wiff_core::session::{SessionLog, SessionWatcher, read_records};
    use wiff_core::{
        CapturedDiff, DraftComment, LockWait, ProjectIdentity, RefreshOutcome, ReviewState,
        ScmType, create_session,
    };
    use wiff_diff::{LineNo, Side};
    use wiff_tui::{
        Action, App, CommentSync, CompareRequest, DiffView, Key, KeyPress, Review, Theme,
    };

    use super::{
        commit_drafts, compare_in_place, recapture, refresh_in_place, refresh_report,
        reload_committed, save_in_place, source_changed, sync_report,
    };
    use crate::command::{DiffSelection, capture_scm_diff};

    /// The human reviewer these tests attribute drafts to.
    fn wez() -> Author {
        Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        }
    }

    /// A bare session header from `source`, for exercising recapture routing.
    fn source_header(source: SourceKind) -> SessionHeader {
        SessionHeader {
            ulid: Ulid(1),
            version: wiff_core::record::FORMAT_VERSION,
            project: "demo".to_string(),
            repo_root: Some("/repos/demo".to_string()),
            cwd: "/repos/demo".to_string(),
            source,
        }
    }

    /// A session header for `ulid`, the first record of a fresh log.
    fn header(ulid: Ulid) -> RecordBody {
        RecordBody::Session(SessionHeader {
            ulid,
            version: wiff_core::record::FORMAT_VERSION,
            project: "demo".to_string(),
            repo_root: Some("/repos/demo".to_string()),
            cwd: "/repos/demo".to_string(),
            source: SourceKind::Stdin,
        })
    }

    #[test]
    fn committing_drafts_appends_them_to_the_session_log_in_order() {
        let base = tempfile::tempdir().expect("tempdir");
        let (log, lock) = SessionLog::create(base.path(), "demo", header).expect("create");
        let path = log.path().to_path_buf();
        let ulid = log.ulid();
        drop(lock);
        drop(log);

        let drafts = vec![
            resolve_event(Ulid(1), wez(), true),
            delete_event(Ulid(2), wez()),
        ];
        commit_drafts(&path, drafts).expect("commit");

        // The header is followed by the two drafts in the order they were made;
        // the non-deterministic `at` timestamp is dropped from the comparison.
        let records = read_records(&path).expect("read");
        let got: Vec<(u64, RecordBody)> = records
            .into_iter()
            .map(|record| (record.seq.get(), record.body))
            .collect();
        wince::assert_eq!(
            got,
            vec![
                (0, header(ulid)),
                (1, resolve_event(Ulid(1), wez(), true)),
                (2, delete_event(Ulid(2), wez())),
            ]
        );
    }

    #[test]
    fn committing_no_drafts_leaves_the_log_untouched() {
        let base = tempfile::tempdir().expect("tempdir");
        let (log, lock) = SessionLog::create(base.path(), "demo", header).expect("create");
        let path = log.path().to_path_buf();
        let ulid = log.ulid();
        drop(lock);
        drop(log);

        commit_drafts(&path, Vec::new()).expect("commit");

        let records = read_records(&path).expect("read");
        let got: Vec<(u64, RecordBody)> = records
            .into_iter()
            .map(|record| (record.seq.get(), record.body))
            .collect();
        wince::assert_eq!(got, vec![(0, header(ulid))]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stdin_session_cannot_be_recaptured_in_the_tui() {
        // Stdin is the terminal once the TUI is open, so a stdin-sourced session
        // is directed to the `wiff refresh` command instead of being reread.
        // The recapture blocks on the runtime, so it needs one even though the
        // stdin arm never reaches git.
        let error = recapture(&source_header(SourceKind::Stdin)).unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "this session's diff came from stdin; refresh it with `wiff refresh` and a new piped diff"
                .to_string()
        );
    }

    #[test]
    fn the_refresh_report_tallies_the_captured_version_and_comments() {
        let report = refresh_report(&RefreshOutcome {
            version: VersionNumber(3),
            exact: 2,
            approximate: 1,
            relocated: 1,
            outdated: 0,
        });
        wince::assert_eq!(
            report,
            "captured v3; rebased 4 comments: 2 exact, 1 shifted, 1 moved, 0 outdated".to_string()
        );
    }

    /// Run `git` with `args` in `repo`, failing loudly on a nonzero status. Uses
    /// a fixed identity and disables signing so the setup is deterministic and
    /// independent of the host's git configuration.
    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(["-c", "user.name=wez", "-c", "user.email=wez@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The plain text a reviewer sees on `app`'s screen at `width`: every visible
    /// line with its trailing highlight padding trimmed, then the status line.
    fn screen(app: &App, width: usize) -> String {
        let mut out = String::new();
        for line in app.visible(width) {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            out.push_str(text.trim_end());
            out.push('\n');
        }
        out.push_str("---\n");
        let status: String = app
            .status(width)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        out.push_str(status.trim_end());
        out.push('\n');
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refreshing_recaptures_the_working_tree_and_rebases_a_comment() {
        // A real git repo backs the session: a committed base, a working-tree
        // change captured as v0, a comment anchored to the added line, then a
        // further change. Refreshing recaptures git, rebases the comment onto
        // the new diff, and reloads the review over it.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // The working tree gains a fourth line; this is the diff v0 captures.
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::Worktree,
        )
        .await
        .expect("capture v0");
        let mut log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        DraftComment {
            author: Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            target: CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(4).unwrap(),
                end_line: LineNo::new(4).unwrap(),
            },
            body: "why delta?".to_string(),
            disposition: None,
        }
        .append(&mut log, LockWait::Block)
        .expect("attach comment");
        drop(log);

        // The working tree gains a line at the top too, so delta slides down and
        // the comment must rebase from line 4 to line 5.
        std::fs::write(&file, "zero\nalpha\nbeta\ngamma\ndelta\n").expect("write v1");

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version.get(),
            state.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        refresh_in_place(
            &session_path,
            &author,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
        )
        .expect("refresh in place");

        // The reloaded review shows the full recaptured v1 diff, with the comment
        // rebased above the added delta on its new line, and the status line
        // reports the tally. A version-comparison prompt is offered over the full
        // diff, opening on the latest where the reviewer was reading.
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  f.txt\n",
            "@@ -1,3 +1,5 @@\n",
            "        1 + zero\n",
            "   1    2   alpha\n",
            "   2    3   beta\n",
            "   3    4   gamma\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why delta?                                                                    │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        5 +└delta\n",
            "---\n",
            "captured v1; rebased 1 comment: 1 exact, 0 shifted, 0 moved, 0 outdated\n",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn comparing_against_an_earlier_version_shows_the_change_since_then() {
        // A committed base, a working change captured as v0, then a further
        // change refreshed into v1. Comparing the review against v0 reconstructs
        // the change made between the two versions -- beta becoming BETA -- with
        // the still-present delta as context, rather than the whole v1 diff.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // v0 appends delta to the working tree.
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::Worktree,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version.get(),
            state.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        // v1 rewrites beta in the working tree; refreshing captures it.
        std::fs::write(&file, "alpha\nBETA\ngamma\ndelta\n").expect("write v1");
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        refresh_in_place(
            &session_path,
            &author,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
        )
        .expect("refresh to v1");

        compare_in_place(
            &session_path,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
            CompareRequest::Version(0),
        )
        .expect("compare");

        // The review now shows only what changed between v0 and v1: beta on the
        // left, BETA on the right, with the unchanged lines as context, and the
        // status line names the comparison.
        let expected = "\
Review [press c here to draft the review comment] [press e to write the description]
modified  f.txt
@@ -1,4 +1,4 @@
   1    1   alpha
   2      - beta
        2 + BETA
   3    3   gamma
   4    4   delta
---
comparing v0 against v1
";
        wince::assert_eq!(screen(&app, 80), expected.to_string());

        // Returning to the latest diff shows v1's own captured change against
        // its baseline again.
        compare_in_place(
            &session_path,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
            CompareRequest::Latest,
        )
        .expect("back to latest");
        let latest = "\
Review [press c here to draft the review comment] [press e to write the description]
modified  f.txt
@@ -1,3 +1,4 @@
   1    1   alpha
   2      - beta
        2 + BETA
   3    3   gamma
        4 + delta
---
showing the latest diff (v1)
";
        wince::assert_eq!(screen(&app, 80), latest.to_string());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resumed_session_detects_when_its_source_has_moved_on() {
        // A committed base captured as v0. With the working tree untouched since
        // capture, recapturing matches v0 and nothing is offered; changing the
        // working tree makes the recapture differ, which the launch check
        // reports so a resume can prompt to refresh.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::Worktree,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let state = ReviewState::load(&session_path).expect("load state");
        wince::assert_eq!(source_changed(&state), false);

        // The working tree gains another line, so a recapture no longer matches
        // v0.
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\nepsilon\n").expect("write change");
        let state = ReviewState::load(&session_path).expect("reload state");
        wince::assert_eq!(source_changed(&state), true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stdin_session_never_offers_a_refresh_on_resume() {
        // A stdin diff cannot be recaptured once the TUI owns the terminal, so
        // resuming such a session never reports its source as changed.
        let data = tempfile::tempdir().expect("data tempdir");
        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(data.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = CapturedDiff {
            text: "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,1 @@
+alpha
"
            .to_string(),
            source: SourceKind::Stdin,
            base_revision: None,
            head_revision: None,
        };
        let log = create_session(data.path(), &identity, data.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let state = ReviewState::load(&session_path).expect("load state");
        wince::assert_eq!(source_changed(&state), false);
    }

    #[test]
    fn saving_commits_the_pending_drafts_and_reloads_them_as_committed() {
        // A session over a one-file diff, with a comment drafted in the TUI but
        // not yet committed. Saving appends it to the log and reloads it as a
        // committed comment, so the review keeps editing without the draft.
        let data = tempfile::tempdir().expect("data tempdir");
        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(data.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = CapturedDiff {
            text: "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+alpha
+beta
+gamma
+delta
"
            .to_string(),
            source: SourceKind::Stdin,
            base_revision: None,
            head_revision: None,
        };
        let log = create_session(data.path(), &identity, data.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("view"),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version.get(),
            state.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        // Land on the first content line (alpha) and draft a comment there.
        app.update(Action::Top);
        for _ in 0..3 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for c in "why alpha?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        app.compose_key(KeyPress::with_modifiers(Key::Char('d'), true, false, false));

        save_in_place(&session_path, &mut app).expect("save in place");

        // The draft is now a persisted Comment record on the added alpha line.
        let targets: Vec<CommentTarget> = read_records(&session_path)
            .expect("read")
            .into_iter()
            .filter_map(|record| match record.body {
                RecordBody::CommentEvent(CommentEvent {
                    kind: CommentEventKind::Create(create),
                    ..
                }) => Some(create.target),
                _ => None,
            })
            .collect();
        wince::assert_eq!(
            targets,
            vec![CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(1).unwrap(),
                end_line: LineNo::new(1).unwrap(),
            }]
        );

        // The reloaded review shows the comment as committed (no draft badge)
        // above the alpha line, and the status line reports the commit.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "added  f.txt\n",
            "@@ -0,0 +1,4 @@\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why alpha?                                                                    │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        1 +└alpha\n",
            "        2 + beta\n",
            "        3 + gamma\n",
            "        4 + delta\n",
            "---\n",
            "committed 1 change\n",
        );
    }

    #[test]
    fn syncing_picks_up_a_comment_committed_by_another_actor() {
        // A session over a one-file diff, opened with no comments. While it is
        // being reviewed an agent commits a comment on the first added line; the
        // watcher registers the append, the reload folds it in as a committed
        // comment, and the tally reports one added.
        let data = tempfile::tempdir().expect("data tempdir");
        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(data.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = CapturedDiff {
            text: "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+alpha
+beta
+gamma
+delta
"
            .to_string(),
            source: SourceKind::Stdin,
            base_revision: None,
            head_revision: None,
        };
        let log = create_session(data.path(), &identity, data.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("view"),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version.get(),
            state.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        // The watcher takes the freshly created session as its baseline, so it
        // registers no change until another actor writes.
        let mut watcher = SessionWatcher::new(&session_path);
        wince::assert_eq!(watcher.changed().is_some(), false);

        // An agent commits a comment on the alpha line straight to the log.
        let mut log = SessionLog::open(&session_path).expect("open");
        log.append_locked(RecordBody::CommentEvent(CommentEvent {
            id: Ulid(7),
            author: Author {
                name: "assistant".to_string(),
                kind: AuthorKind::Agent,
            },
            authored_at: None,
            origin: None,
            synced_marker: None,
            kind: CommentEventKind::Create(CommentCreate {
                target: CommentTarget::Lines {
                    file: "f.txt".to_string(),
                    side: Side::After,
                    start_line: LineNo::new(1).unwrap(),
                    end_line: LineNo::new(1).unwrap(),
                },
                version,
                anchor: None,
                body: "alpha looks off".to_string(),
                disposition: None,
            }),
        }))
        .expect("append comment");

        let fingerprint = watcher.changed().expect("the append registers");
        let summary = reload_committed(&session_path, &mut app).expect("reload");
        watcher.acknowledge(fingerprint);
        app.set_message(sync_report(&summary));

        wince::assert_eq!(
            summary,
            CommentSync {
                added: 1,
                changed: 0,
                description_changed: false,
                removed: 0,
            }
        );
        // The acknowledged change no longer registers.
        wince::assert_eq!(watcher.changed().is_some(), false);

        // The agent's comment now shows as committed above the alpha line, and
        // the status line reports what was synced.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "added  f.txt\n",
            "@@ -0,0 +1,4 @@\n",
            "┌ #1 assistant (agent)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│alpha looks off                                                               │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        1 +└alpha\n",
            "        2 + beta\n",
            "        3 + gamma\n",
            "        4 + delta\n",
            "---\n",
            "synced: 1 added\n",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_comment_drafted_in_the_tui_renders_its_snippet_after_commit() {
        // A reviewer highlights a changed line in the TUI and drafts a comment
        // on it. Committing captures the line's anchor, so `wiff render` shows
        // the fenced snippet the same as a comment added through the CLI.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // The working tree changes the middle line; this is the diff v0 captures.
        std::fs::write(&file, "alpha\nBETA\ngamma\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::Worktree,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let mut review = Review::new(
            DiffView::new(theme).expect("renderer"),
            diff,
            wez(),
            version.get(),
            state.comments.clone(),
            None,
        );

        // The reviewer drafts a comment on the changed line, then commits it.
        review.add_comment(
            CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(2).unwrap(),
                end_line: LineNo::new(2).unwrap(),
            },
            "why uppercase?".to_string(),
        );
        let drafts = review.take_drafts();
        commit_drafts(&session_path, drafts).expect("commit");

        // The rendered review shows the comment with its captured snippet: the
        // changed line marked, with the line above and below as context. The
        // session's ulid is variable, so it is normalized before the comparison.
        let state = ReviewState::load(&session_path).expect("reload state");
        let rendered = crate::render::render(&state, crate::render::Format::Markdown)
            .expect("render markdown");
        let normalized = rendered.replace(&state.session.ulid.to_string(), "SESSION");
        #[rustfmt::skip]
        wince::snapshot_str!(
            normalized,
            "# Review SESSION\n",
            "\n",
            "- project: demo\n",
            "- source: git worktree\n",
            "- version: v0 (1 file)\n",
            "\n",
            "## Comments\n",
            "\n",
            "### f.txt\n",
            "\n",
            "- #1 line 2 (after) by wez (human)\n",
            "  why uppercase?\n",
            "\n",
            "  ```\n",
            "       1 | alpha\n",
            "  >    2 | BETA\n",
            "       3 | gamma\n",
            "  ```\n",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_draft_refreshed_before_commit_captures_its_anchor_from_the_new_version() {
        // A comment is drafted against v0, then a refresh recaptures the working
        // tree as v1 and rebases the draft forward before it is committed. The
        // commit must capture the anchor from v1, where the draft now lives, so
        // the rendered snippet shows the reviewed line at its v1 position with
        // its v1 context rather than v0's.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // v0 appends delta; the draft comments on it at line 4.
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::Worktree,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let mut review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            wez(),
            version.get(),
            state.comments.clone(),
            None,
        );
        review.add_comment(
            CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(4).unwrap(),
                end_line: LineNo::new(4).unwrap(),
            },
            "why delta?".to_string(),
        );
        let mut app = App::reviewing(review, 40, &theme);

        // A line inserted at the top slides delta from line 4 to line 5; the
        // refresh recaptures this as v1 and rebases the draft onto it.
        std::fs::write(&file, "zero\nalpha\nbeta\ngamma\ndelta\n").expect("write v1");
        refresh_in_place(
            &session_path,
            &wez(),
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
        )
        .expect("refresh in place");

        let drafts = app.draft_records();
        commit_drafts(&session_path, drafts).expect("commit");

        // The committed comment renders its anchor from v1: delta at line 5 with
        // gamma above it, proving the capture read the version the draft rebased
        // onto rather than the v0 it was authored against.
        let state = ReviewState::load(&session_path).expect("reload state");
        let rendered = crate::render::render(&state, crate::render::Format::Markdown)
            .expect("render markdown");
        let normalized = rendered.replace(&state.session.ulid.to_string(), "SESSION");
        #[rustfmt::skip]
        wince::snapshot_str!(
            normalized,
            "# Review SESSION\n",
            "\n",
            "- project: demo\n",
            "- source: git worktree\n",
            "- version: v1 (1 file)\n",
            "\n",
            "## Comments\n",
            "\n",
            "### f.txt\n",
            "\n",
            "- #1 line 5 (after) by wez (human)\n",
            "  why delta?\n",
            "\n",
            "  ```\n",
            "       2 | alpha\n",
            "       3 | beta\n",
            "       4 | gamma\n",
            "  >    5 | delta\n",
            "  ```\n",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saving_a_draft_whose_diff_is_gone_reports_the_comment_as_unanchored() {
        // A comment is drafted against v0, then that version's sideband diff is
        // removed before the reviewer saves. The commit cannot read the diff to
        // capture the anchor, so the comment commits as a bare locator and the
        // status line reports it as unanchored without failing the save.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(&file, "alpha\nBETA\ngamma\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::Worktree,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let mut review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            wez(),
            version.get(),
            state.comments.clone(),
            None,
        );
        review.add_comment(
            CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(2).unwrap(),
                end_line: LineNo::new(2).unwrap(),
            },
            "why uppercase?".to_string(),
        );
        let mut app = App::reviewing(review, 40, &theme);

        // Remove the sideband diff the anchor would be captured from.
        let diff_path = SessionLog::open(&session_path)
            .unwrap()
            .sideband_dir()
            .join(format!("v{version}.diff"));
        std::fs::remove_file(&diff_path).expect("remove sideband diff");

        save_in_place(&session_path, &mut app).expect("save");

        // The comment commits and shows above the reviewed line; the status line
        // reports the one comment that could not be anchored as a count.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  f.txt\n",
            "@@ -1,3 +1,3 @@\n",
            "   1    1   alpha\n",
            "   2      - beta\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why uppercase?                                                                │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        2 +└BETA\n",
            "   3    3   gamma\n",
            "---\n",
            "committed 1 change; 1 comment could not be anchored\n",
        );

        // The comment persisted as a bare locator, without an anchor.
        let anchors: Vec<Option<wiff_core::record::Anchor>> = ReviewState::load(&session_path)
            .expect("reload state")
            .comments
            .iter()
            .map(|comment| comment.anchor.clone())
            .collect();
        wince::assert_eq!(anchors, vec![None]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saving_two_drafts_whose_diff_is_gone_reports_the_plural_count() {
        // Two comments are drafted against v0, then that version's sideband diff
        // is removed before the reviewer saves. Both commit as bare locators and
        // the status line pluralizes the unanchored count.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(&file, "ALPHA\nBETA\ngamma\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::Worktree,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let mut review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            wez(),
            version.get(),
            state.comments.clone(),
            None,
        );
        for line in [1u32, 2] {
            review.add_comment(
                CommentTarget::Lines {
                    file: "f.txt".to_string(),
                    side: Side::After,
                    start_line: LineNo::new(line).unwrap(),
                    end_line: LineNo::new(line).unwrap(),
                },
                format!("why line {line}?"),
            );
        }
        let mut app = App::reviewing(review, 40, &theme);

        // Remove the sideband diff both anchors would be captured from.
        let diff_path = SessionLog::open(&session_path)
            .unwrap()
            .sideband_dir()
            .join(format!("v{version}.diff"));
        std::fs::remove_file(&diff_path).expect("remove sideband diff");

        save_in_place(&session_path, &mut app).expect("save");

        // Both comments commit and the status line reports the plural count.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  f.txt\n",
            "@@ -1,3 +1,3 @@\n",
            "   1      - alpha\n",
            "   2      - beta\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why line 1?                                                                   │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        1 +└ALPHA\n",
            "┌ #2 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why line 2?                                                                   │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        2 +└BETA\n",
            "   3    3   gamma\n",
            "---\n",
            "committed 2 changes; 2 comments could not be anchored\n",
        );

        // Both comments persisted as bare locators, without anchors.
        let anchors: Vec<Option<wiff_core::record::Anchor>> = ReviewState::load(&session_path)
            .expect("reload state")
            .comments
            .iter()
            .map(|comment| comment.anchor.clone())
            .collect();
        wince::assert_eq!(anchors, vec![None, None]);
    }
}
