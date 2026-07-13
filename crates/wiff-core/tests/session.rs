#![allow(missing_docs)]

use ulid::Ulid;
use wiff_core::record::{
    Author, AuthorKind, CommentRecord, CommentTarget, RecordBody, SessionHeader, SourceKind,
};
use wiff_core::session::{
    LockAttempt, SessionLog, SyncState, active_session, list_projects, list_sessions, read_records,
    remove_session,
};
use wiff_diff::{LineNo, Side};

fn header(ulid: Ulid) -> RecordBody {
    RecordBody::Session(SessionHeader {
        ulid,
        version: wiff_core::record::FORMAT_VERSION,
        project: "demo".to_string(),
        repo_root: Some("/repos/demo".to_string()),
        cwd: "/repos/demo/sub".to_string(),
        source: SourceKind::GitWorktree,
    })
}

fn comment() -> RecordBody {
    RecordBody::Comment(CommentRecord {
        id: Ulid::from_string("00000000000000000000000000").unwrap(),
        author: Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        },
        target: CommentTarget::Lines {
            file: "src/main.rs".to_string(),
            side: Side::After,
            start_line: LineNo::new(2).unwrap(),
            end_line: LineNo::new(2).unwrap(),
        },
        version: 0,
        anchor: None,
        body: "why 3?".to_string(),
    })
}

#[test]
fn create_append_and_read_round_trip() {
    let base = tempfile::tempdir().unwrap();
    let (mut log, mut lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let seq = log.append(&mut lock, comment()).unwrap();
    wince::assert_eq!(seq, 1);
    drop(lock);

    let records = read_records(log.path()).unwrap();
    // Compare the full (seq, body) of every record in one assertion; the `at`
    // timestamp is the only non-deterministic field and is excluded.
    let got: Vec<(u64, RecordBody)> = records
        .into_iter()
        .map(|record| (record.seq, record.body))
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
    wince::assert_eq!(seq, 1);
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
fn a_locked_batch_appends_every_record_in_order() {
    let base = tempfile::tempdir().unwrap();
    let (log, lock) = SessionLog::create(base.path(), "demo", header).unwrap();
    let path = log.path().to_path_buf();
    drop(lock);

    let mut reopened = SessionLog::open(&path).unwrap();
    let seqs = reopened
        .append_all_locked(vec![comment(), comment()])
        .unwrap();
    wince::assert_eq!(seqs, vec![1, 2]);
    let got: Vec<(u64, RecordBody)> = read_records(&path)
        .unwrap()
        .into_iter()
        .map(|record| (record.seq, record.body))
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
        .map(|record| (record.seq, record.body))
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
    wince::assert_eq!(
        active_session(base.path(), "demo").unwrap(),
        second.path().to_path_buf()
    );
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
