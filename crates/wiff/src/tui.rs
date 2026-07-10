//! Opening a session in the review TUI.
//!
//! This is the bridge from the persisted session to the interactive review: it
//! loads the latest captured diff, renders it, and runs the terminal loop, then
//! keeps or removes the session according to how the reviewer chose to leave.

use std::path::Path;

use anyhow::Context;
use wiff_config::{Config, OnExit};
use wiff_core::record::{AuthorKind, RecordBody, SessionHeader};
use wiff_core::session::{SessionWatcher, remove_session};
use wiff_core::{RefreshOutcome, ReviewState, SessionLog, refresh_session};
use wiff_tui::{App, CommentSync, DiffView, Exit, ExitDefault, KeyHints, Review, Theme, run};

use crate::command::recapture_diff;

/// Open `session_path` in the review TUI, then keep or remove the session per
/// the reviewer's choice and the configured `on_exit` default.
pub fn open(session_path: &Path, config: &Config) -> anyhow::Result<()> {
    let log = SessionLog::open(session_path)?;
    let state = ReviewState::load(session_path)?;
    let version = state
        .latest_version()
        .context("this session has no captured diff to review")?;
    let text = log.read_diff(version.number)?;
    let diff = wiff_diff::parse(&text)?;

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
    let review = Review::new(view, diff, author, version.number, comments);
    let app = App::reviewing(review, 0, &theme)
        .with_exit_default(exit_default(config.on_exit))
        .with_keymap(keymap.clone());

    // Refresh recaptures the diff and reloads the app in place; save commits the
    // pending drafts and keeps the review open. Any failure is reported in the
    // status line rather than tearing down the review.
    let refresh = |app: &mut App| {
        if let Err(err) = refresh_in_place(session_path, app) {
            app.set_message(format!("refresh failed: {err}"));
        }
    };
    let save = |app: &mut App| {
        if let Err(err) = save_in_place(session_path, app) {
            app.set_message(format!("save failed: {err}"));
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

    let (exit, drafts) = run(app, keymap, refresh, save, sync)?;
    resolve_exit(exit, session_path, drafts)
}

/// Recapture the session's diff as a new version, rebase its committed comments
/// and the reviewer's pending drafts onto it, and reload `app` over the result,
/// reporting the tally in the status line. A no-op capture (nothing changed)
/// says so instead.
fn refresh_in_place(session_path: &Path, app: &mut App) -> anyhow::Result<()> {
    let state = ReviewState::load(session_path)?;
    let diff_text = recapture(&state.session)?;
    let mut log = SessionLog::open(session_path)?;
    let outcome = match refresh_session(&mut log, &diff_text)? {
        Some(outcome) => outcome,
        None => {
            let current = state.latest_version().map(|v| v.number).unwrap_or(0);
            app.set_message(format!("no changes since v{current}"));
            return Ok(());
        }
    };

    let state = ReviewState::load(session_path)?;
    let version = state
        .latest_version()
        .context("the refreshed session has no captured diff")?;
    let diff = wiff_diff::parse(&log.read_diff(version.number)?)?;
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    app.refresh(diff, comments, version.number, |authored_version| {
        Ok(wiff_diff::parse(&log.read_diff(authored_version)?)?)
    })?;
    app.set_message(refresh_report(&outcome));
    Ok(())
}

/// Commit the reviewer's pending drafts to the session log and reload the
/// review's committed comments over them, keeping the review open. Reports the
/// tally in the status line; a no-op when nothing is pending.
fn save_in_place(session_path: &Path, app: &mut App) -> anyhow::Result<()> {
    let drafts = app.take_drafts();
    if drafts.is_empty() {
        app.set_message("nothing to save".to_string());
        return Ok(());
    }
    let count = drafts.len();
    commit_drafts(session_path, drafts)?;
    reload_committed(session_path, app)?;
    app.set_message(format!(
        "committed {count} change{}",
        if count == 1 { "" } else { "s" }
    ));
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
    Ok(app.reload_comments(comments))
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
    format!("synced: {}", parts.join(", "))
}

/// Recapture the diff from the session's original source. A stdin source cannot
/// be reread inside the TUI, since stdin is now the terminal, so it is directed
/// to the `wiff refresh` command instead.
fn recapture(header: &SessionHeader) -> anyhow::Result<String> {
    // The event loop runs on a tokio worker, so block on the async recapture
    // without standing up a nested runtime.
    let text = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(recapture_diff(header))
    })?;
    text.context(
        "this session's diff came from stdin; refresh it with `wiff refresh` and a new piped diff",
    )
}

/// The status-line tally of a refresh: the captured version and how its comments
/// fared.
fn refresh_report(outcome: &RefreshOutcome) -> String {
    let total = outcome.exact + outcome.approximate + outcome.outdated;
    format!(
        "captured v{}; rebased {total} comment{}: {} exact, {} shifted, {} outdated",
        outcome.version,
        if total == 1 { "" } else { "s" },
        outcome.exact,
        outcome.approximate,
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
            commit_drafts(session_path, drafts)?;
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

/// Append the reviewer's buffered draft edits to the session log in order, each
/// taking the file lock transiently. Does nothing when there are no drafts.
fn commit_drafts(session_path: &Path, drafts: Vec<RecordBody>) -> anyhow::Result<()> {
    if drafts.is_empty() {
        return Ok(());
    }
    let mut log = SessionLog::open(session_path)?;
    for body in drafts {
        log.append_locked(body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use ulid::Ulid;
    use wiff_core::record::{
        Author, AuthorKind, CommentDelete, CommentRecord, CommentResolve, CommentTarget,
        RecordBody, SessionHeader, SourceKind,
    };
    use wiff_core::session::{SessionLog, SessionWatcher, read_records};
    use wiff_core::{
        CapturedDiff, DraftComment, ProjectIdentity, RefreshOutcome, ReviewState, ScmType,
        create_session,
    };
    use wiff_diff::{LineNo, Side};
    use wiff_tui::{Action, App, CommentSync, DiffView, Key, KeyPress, Review, Theme};

    use super::{
        commit_drafts, recapture, refresh_in_place, refresh_report, reload_committed,
        save_in_place, sync_report,
    };
    use crate::command::{DiffSelection, capture_scm_diff};

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
            source: SourceKind::GitWorktree,
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
            RecordBody::CommentResolve(CommentResolve {
                id: Ulid(1),
                resolved: true,
                author: Author {
                    name: "wez".to_string(),
                    kind: AuthorKind::Human,
                },
            }),
            RecordBody::CommentDelete(CommentDelete {
                id: Ulid(2),
                author: Author {
                    name: "wez".to_string(),
                    kind: AuthorKind::Human,
                },
            }),
        ];
        commit_drafts(&path, drafts).expect("commit");

        // The header is followed by the two drafts in the order they were made;
        // the non-deterministic `at` timestamp is dropped from the comparison.
        let records = read_records(&path).expect("read");
        let got: Vec<(u64, RecordBody)> = records
            .into_iter()
            .map(|record| (record.seq, record.body))
            .collect();
        k9::assert_equal!(
            got,
            vec![
                (0, header(ulid)),
                (
                    1,
                    RecordBody::CommentResolve(CommentResolve {
                        id: Ulid(1),
                        resolved: true,
                        author: Author {
                            name: "wez".to_string(),
                            kind: AuthorKind::Human,
                        },
                    })
                ),
                (
                    2,
                    RecordBody::CommentDelete(CommentDelete {
                        id: Ulid(2),
                        author: Author {
                            name: "wez".to_string(),
                            kind: AuthorKind::Human,
                        },
                    })
                ),
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
            .map(|record| (record.seq, record.body))
            .collect();
        k9::assert_equal!(got, vec![(0, header(ulid))]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stdin_session_cannot_be_recaptured_in_the_tui() {
        // Stdin is the terminal once the TUI is open, so a stdin-sourced session
        // is directed to the `wiff refresh` command instead of being reread.
        // The recapture blocks on the runtime, so it needs one even though the
        // stdin arm never reaches git.
        let error = recapture(&source_header(SourceKind::Stdin)).unwrap_err();
        k9::assert_equal!(
            error.to_string(),
            "this session's diff came from stdin; refresh it with `wiff refresh` and a new piped diff"
                .to_string()
        );
    }

    #[test]
    fn the_refresh_report_tallies_the_captured_version_and_comments() {
        let report = refresh_report(&RefreshOutcome {
            version: 3,
            exact: 2,
            approximate: 1,
            outdated: 0,
        });
        k9::assert_equal!(
            report,
            "captured v3; rebased 3 comments: 2 exact, 1 shifted, 0 outdated".to_string()
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
        let mut log =
            create_session(data.path(), &identity, repo.path(), &captured).expect("create session");
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
        }
        .append(&mut log)
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
            version,
            state.comments.clone(),
        );
        let mut app = App::reviewing(review, 40, &theme);

        refresh_in_place(&session_path, &mut app).expect("refresh in place");

        // The reloaded review shows the recaptured v1 diff, with the comment
        // rebased above the added delta on its new line, and the status line
        // reports the tally.
        let expected = "\
Review [press c here to draft the review comment]
modified  f.txt
@@ -1,3 +1,5 @@
        1 + zero
   1    2   alpha
   2    3   beta
   3    4   gamma
┌ wez (human)  press e to edit  r to resolve  d to delete ─────────────────────┐
│why delta?                                                                    │
└──────────────────────────────────────────────────────────────────────────────┘
        5 + delta
---
captured v1; rebased 1 comment: 1 exact, 0 shifted, 0 outdated
";
        k9::assert_equal!(screen(&app, 80), expected.to_string());
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
            source: SourceKind::GitWorktree,
        };
        let log =
            create_session(data.path(), &identity, data.path(), &captured).expect("create session");
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
            version,
            state.comments.clone(),
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
                RecordBody::Comment(comment) => Some(comment.target),
                _ => None,
            })
            .collect();
        k9::assert_equal!(
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
        let expected = "\
Review [press c here to draft the review comment]
added  f.txt
@@ -0,0 +1,4 @@
┌ wez (human)  press e to edit  r to resolve  d to delete ─────────────────────┐
│why alpha?                                                                    │
└──────────────────────────────────────────────────────────────────────────────┘
        1 + alpha
        2 + beta
        3 + gamma
        4 + delta
---
committed 1 change
";
        k9::assert_equal!(screen(&app, 80), expected.to_string());
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
            source: SourceKind::GitWorktree,
        };
        let log =
            create_session(data.path(), &identity, data.path(), &captured).expect("create session");
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
            version,
            state.comments.clone(),
        );
        let mut app = App::reviewing(review, 40, &theme);

        // The watcher takes the freshly created session as its baseline, so it
        // registers no change until another actor writes.
        let mut watcher = SessionWatcher::new(&session_path);
        k9::assert_equal!(watcher.changed().is_some(), false);

        // An agent commits a comment on the alpha line straight to the log.
        let mut log = SessionLog::open(&session_path).expect("open");
        log.append_locked(RecordBody::Comment(CommentRecord {
            id: Ulid(7),
            author: Author {
                name: "assistant".to_string(),
                kind: AuthorKind::Agent,
            },
            target: CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(1).unwrap(),
                end_line: LineNo::new(1).unwrap(),
            },
            version,
            anchor: None,
            body: "alpha looks off".to_string(),
        }))
        .expect("append comment");

        let fingerprint = watcher.changed().expect("the append registers");
        let summary = reload_committed(&session_path, &mut app).expect("reload");
        watcher.acknowledge(fingerprint);
        app.set_message(sync_report(&summary));

        k9::assert_equal!(
            summary,
            CommentSync {
                added: 1,
                changed: 0,
                removed: 0,
            }
        );
        // The acknowledged change no longer registers.
        k9::assert_equal!(watcher.changed().is_some(), false);

        // The agent's comment now shows as committed above the alpha line, and
        // the status line reports what was synced.
        let expected = "\
Review [press c here to draft the review comment]
added  f.txt
@@ -0,0 +1,4 @@
┌ assistant (agent)  press e to edit  r to resolve  d to delete ───────────────┐
│alpha looks off                                                               │
└──────────────────────────────────────────────────────────────────────────────┘
        1 + alpha
        2 + beta
        3 + gamma
        4 + delta
---
synced: 1 added
";
        k9::assert_equal!(screen(&app, 80), expected.to_string());
    }
}
