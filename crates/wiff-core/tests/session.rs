#![allow(missing_docs)]

use ulid::Ulid;
use wiff_core::record::{
    Author, AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentTarget, RecordBody,
    ScmSource, Seq, SessionHeader, SourceKind, TipRule, VersionNumber,
};
use wiff_core::session::{
    LockAttempt, LockWait, SessionLog, SyncState, active_session, list_projects, list_sessions,
    read_records, remove_session,
};
use wiff_core::{BaseRuleset, ScmType};
use wiff_diff::{LineNo, Side};

fn header(ulid: Ulid) -> RecordBody {
    RecordBody::Session(SessionHeader {
        ulid,
        version: wiff_core::record::FORMAT_VERSION,
        project: "demo".to_string(),
        repo_root: Some("/repos/demo".to_string()),
        cwd: "/repos/demo/sub".to_string(),
        source: SourceKind::Scm(ScmSource {
            scm: ScmType::Git,
            base: BaseRuleset::new("ref(name(deadbeef))"),
            tip: TipRule::Worktree,
            branch_hint: None,
        }),
    })
}

fn comment() -> RecordBody {
    RecordBody::CommentEvent(CommentEvent {
        id: Ulid::from_string("00000000000000000000000000").unwrap(),
        author: Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        },
        authored_at: None,
        origin: None,
        synced_marker: None,
        kind: CommentEventKind::Create(CommentCreate {
            target: CommentTarget::Lines {
                file: "src/main.rs".to_string(),
                side: Side::After,
                start_line: LineNo::new(2).unwrap(),
                end_line: LineNo::new(2).unwrap(),
            },
            version: VersionNumber(0),
            anchor: None,
            body: "why 3?".to_string(),
            disposition: None,
        }),
    })
}

#[test]
fn create_append_and_read_round_trip() {
    let base = tempfile::tempdir().unwrap();
    let (mut log, mut lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let seq = log.append(&mut lock, comment()).unwrap();
    wince::assert_eq!(seq, Seq(1));
    drop(lock);

    let records = read_records(log.path()).unwrap();
    // Compare the full (seq, body) of every record in one assertion; the `at`
    // timestamp is the only non-deterministic field and is excluded.
    let got: Vec<(u64, RecordBody)> = records
        .into_iter()
        .map(|record| (record.seq.get(), record.body))
        .collect();
    wince::assert_eq!(got, vec![(0, header(log.ulid())), (1, comment())]);
}

#[test]
fn reopen_continues_the_sequence() {
    let base = tempfile::tempdir().unwrap();
    let (log, lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let path = log.path().to_path_buf();
    drop(lock);

    let mut reopened = SessionLog::open(&path).unwrap();
    wince::assert_eq!(reopened.next_seq(), 1);
    let seq = reopened.append_locked(comment()).unwrap();
    wince::assert_eq!(seq, Seq(1));
    wince::assert_eq!(read_records(&path).unwrap().len(), 2);
}

#[test]
fn a_held_lock_makes_a_second_writer_contended() {
    let base = tempfile::tempdir().unwrap();
    let (log, _lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let other = SessionLog::open(log.path()).unwrap();
    match other.lock().unwrap() {
        LockAttempt::Contended => {}
        LockAttempt::Acquired { .. } => panic!("expected the session to be contended"),
    }
}

#[test]
fn appending_from_a_stale_handle_diverges() {
    let base = tempfile::tempdir().unwrap();
    let (mut writer, mut lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let stale = SessionLog::open(writer.path()).unwrap();
    writer.append(&mut lock, comment()).unwrap();
    drop(lock);
    match stale.lock().unwrap() {
        LockAttempt::Acquired {
            sync: SyncState::Diverged { file_next_seq },
            ..
        } => {
            wince::assert_eq!(file_next_seq, 2);
        }
        other => panic!("expected divergence, got {other:?}"),
    }
}

#[test]
fn lock_and_sync_resyncs_a_stale_handle_and_appends_at_the_tail() {
    // A one-shot writer that opened the log before another writer appended can
    // still commit: taking the lock resyncs its position to the file's tail, so
    // the append is placed after the record it had not seen instead of diverging.
    let base = tempfile::tempdir().unwrap();
    let (mut writer, mut lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let mut stale = SessionLog::open(writer.path()).unwrap();
    writer.append(&mut lock, comment()).unwrap();
    drop(lock);

    let (mut guard, _records) = stale.lock_and_sync(LockWait::Block).unwrap();
    let seq = stale.append(&mut guard, comment()).unwrap();
    wince::assert_eq!(seq, Seq(2));
    drop(guard);

    let got: Vec<(u64, RecordBody)> = read_records(writer.path())
        .unwrap()
        .into_iter()
        .map(|record| (record.seq.get(), record.body))
        .collect();
    wince::assert_eq!(
        got,
        vec![(0, header(writer.ulid())), (1, comment()), (2, comment())]
    );
}

#[test]
fn a_locked_batch_appends_every_record_in_order() {
    let base = tempfile::tempdir().unwrap();
    let (log, lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let path = log.path().to_path_buf();
    drop(lock);

    let mut reopened = SessionLog::open(&path).unwrap();
    let seqs = reopened
        .append_all_locked(vec![comment(), comment()])
        .unwrap();
    wince::assert_eq!(seqs, vec![Seq(1), Seq(2)]);
    let got: Vec<(u64, RecordBody)> = read_records(&path)
        .unwrap()
        .into_iter()
        .map(|record| (record.seq.get(), record.body))
        .collect();
    wince::assert_eq!(
        got,
        vec![(0, header(reopened.ulid())), (1, comment()), (2, comment()),]
    );
}

#[test]
fn a_diverged_batch_writes_none_of_its_records() {
    // A stale handle whose position the file has moved past must reject the
    // whole batch without writing any of it, so a retry cannot duplicate a
    // partially written prefix.
    let base = tempfile::tempdir().unwrap();
    let (mut writer, mut lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let mut stale = SessionLog::open(writer.path()).unwrap();
    writer.append(&mut lock, comment()).unwrap();
    drop(lock);

    let outcome = match stale.append_all_locked(vec![comment(), comment()]) {
        Ok(seqs) => format!("wrote {seqs:?}"),
        Err(err) => format!("{err}"),
    };
    wince::assert_eq!(
        outcome,
        format!(
            "session {} diverged from our position",
            writer.path().display()
        )
    );
    let got: Vec<(u64, RecordBody)> = read_records(writer.path())
        .unwrap()
        .into_iter()
        .map(|record| (record.seq.get(), record.body))
        .collect();
    wince::assert_eq!(got, vec![(0, header(writer.ulid())), (1, comment())]);
}

#[test]
fn discovery_lists_projects_sessions_and_the_active_one() {
    let base = tempfile::tempdir().unwrap();
    let (first, first_lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    drop(first_lock);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let (second, second_lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    drop(second_lock);

    wince::assert_eq!(
        list_projects(base.path()).unwrap(),
        vec!["demo".to_string()]
    );
    let sessions = list_sessions(base.path(), "demo").unwrap();
    wince::assert_eq!(
        sessions,
        vec![second.path().to_path_buf(), first.path().to_path_buf()]
    );
    // With no repository context, discovery falls back to the most recent
    // session, which the max(ULID, mtime) recency orders first.
    wince::assert_eq!(
        active_session(base.path(), "demo", None, None).unwrap(),
        second.path().to_path_buf()
    );
}

/// Build a session header with `source`, for the discovery tests.
fn scm_header(source: SourceKind) -> impl FnOnce(Ulid) -> RecordBody {
    move |ulid| {
        RecordBody::Session(SessionHeader {
            ulid,
            version: wiff_core::record::FORMAT_VERSION,
            project: "demo".to_string(),
            repo_root: Some("/repos/demo".to_string()),
            cwd: "/repos/demo".to_string(),
            source,
        })
    }
}

fn ref_source(name: &str) -> SourceKind {
    SourceKind::Scm(ScmSource {
        scm: ScmType::Git,
        base: BaseRuleset::new("parent(@)"),
        tip: TipRule::Ref {
            name: name.to_string(),
        },
        branch_hint: None,
    })
}

fn worktree_source_on(branch: &str) -> SourceKind {
    SourceKind::Scm(ScmSource {
        scm: ScmType::Git,
        base: BaseRuleset::new("ref(name(deadbeef))"),
        tip: TipRule::Worktree,
        branch_hint: Some(branch.to_string()),
    })
}

/// A fresh git repository checked out on `branch`, so discovery has a real
/// `rev-parse --symbolic-full-name HEAD` to read. The git environment is
/// cleared and pointed at an empty HOME so ambient config or `GIT_*` variables
/// cannot perturb the result.
fn git_repo_on_branch(branch: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .env_clear()
            .env("HOME", home.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(["-c", "user.name=t", "-c", "user.email=t@e"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", branch]);
    std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    run(&["add", "f.txt"]);
    run(&["commit", "-q", "-m", "first"]);
    dir
}

#[test]
fn discovery_prefers_the_session_for_the_checked_out_branch() {
    let base = tempfile::tempdir().unwrap();
    let repo = git_repo_on_branch("topic");

    // The session for the checked-out branch is created first (older); a session
    // on another branch is created after it and is the most recent.
    let (matching, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(ref_source("refs/heads/topic")),
    )
    .unwrap();
    drop(lock);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let (_newer, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(ref_source("refs/heads/other")),
    )
    .unwrap();
    drop(lock);

    // Branch match wins over recency: the older matching session is chosen, not
    // the newer one on another branch.
    wince::assert_eq!(
        active_session(base.path(), "demo", Some(repo.path()), Some(ScmType::Git)).unwrap(),
        matching.path().to_path_buf()
    );
}

#[test]
fn discovery_matches_a_working_copy_session_by_its_branch_hint() {
    let base = tempfile::tempdir().unwrap();
    let repo = git_repo_on_branch("topic");

    let (matching, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(worktree_source_on("refs/heads/topic")),
    )
    .unwrap();
    drop(lock);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let (_newer, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(worktree_source_on("refs/heads/other")),
    )
    .unwrap();
    drop(lock);

    wince::assert_eq!(
        active_session(base.path(), "demo", Some(repo.path()), Some(ScmType::Git)).unwrap(),
        matching.path().to_path_buf()
    );
}

#[test]
fn discovery_falls_back_to_the_most_recent_when_no_session_names_the_branch() {
    let base = tempfile::tempdir().unwrap();
    let repo = git_repo_on_branch("topic");

    let (_older, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(ref_source("refs/heads/main")),
    )
    .unwrap();
    drop(lock);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let (newer, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(ref_source("refs/heads/other")),
    )
    .unwrap();
    drop(lock);

    // No session names `topic`, so discovery falls back to the most recent.
    wince::assert_eq!(
        active_session(base.path(), "demo", Some(repo.path()), Some(ScmType::Git)).unwrap(),
        newer.path().to_path_buf()
    );
}

#[test]
fn discovery_skips_a_corrupt_session_to_reach_the_branch_match() {
    let base = tempfile::tempdir().unwrap();
    let repo = git_repo_on_branch("topic");

    // A healthy session on the checked-out branch, and a newer, unrelated
    // session whose header line is committed (newline-terminated) but not a
    // valid record.
    let (matching, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(ref_source("refs/heads/topic")),
    )
    .unwrap();
    drop(lock);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let (corrupt, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(ref_source("refs/heads/other")),
    )
    .unwrap();
    let corrupt_path = corrupt.path().to_path_buf();
    drop(lock);
    std::fs::write(&corrupt_path, "{\"not\":\"a record\"}\n").unwrap();

    // The corrupt session is the most recent, so the scan reaches it first, but
    // skips it rather than aborting and still finds the healthy branch match.
    wince::assert_eq!(
        active_session(base.path(), "demo", Some(repo.path()), Some(ScmType::Git)).unwrap(),
        matching.path().to_path_buf()
    );
}

#[test]
fn discovery_surfaces_a_corrupt_most_recent_when_no_session_names_the_branch() {
    let base = tempfile::tempdir().unwrap();
    let repo = git_repo_on_branch("topic");

    // No session names the checked-out branch, so discovery falls back to the
    // most recent, whose header is corrupt: the caller is about to be pointed at
    // it, so the damage is reported rather than returned silently.
    let (_older, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(ref_source("refs/heads/main")),
    )
    .unwrap();
    drop(lock);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let (corrupt, lock) = SessionLog::create(
        base.path(),
        "demo",
        scm_header(ref_source("refs/heads/other")),
    )
    .unwrap();
    let corrupt_path = corrupt.path().to_path_buf();
    drop(lock);
    std::fs::write(&corrupt_path, "{\"not\":\"a record\"}\n").unwrap();

    let error =
        active_session(base.path(), "demo", Some(repo.path()), Some(ScmType::Git)).unwrap_err();
    wince::assert_eq!(
        error.to_string(),
        format!(
            "could not decode session record: {}",
            decode_error(&corrupt_path)
        )
    );
}

/// The serde error text `active_session` reports for the corrupt header, read
/// back the same way so the assertion does not hard-code serde's wording.
fn decode_error(path: &std::path::Path) -> String {
    let line = std::fs::read_to_string(path).unwrap();
    let line = line.strip_suffix('\n').unwrap();
    serde_json::from_str::<wiff_core::record::Record>(line)
        .unwrap_err()
        .to_string()
}

#[test]
fn removing_a_session_deletes_the_log_and_sideband() {
    let base = tempfile::tempdir().unwrap();
    let (log, lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    drop(lock);
    let sideband = log.sideband_dir();
    std::fs::create_dir_all(&sideband).unwrap();
    std::fs::write(sideband.join("v0.diff"), "diff").unwrap();

    remove_session(log.path()).unwrap();
    wince::assert_eq!(log.path().exists(), false);
    wince::assert_eq!(sideband.exists(), false);
}
