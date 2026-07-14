//! The persistent, append-only session log.
//!
//! A session is a JSON-lines file at `<data>/sessions/<project>/<ULID>.jsonl`,
//! with a sideband `<ULID>.d/` directory beside it. Reads are lock-free and go
//! by path, so they work even while another process is appending. Appends
//! require an exclusive [`flock`](nix::fcntl::Flock), held through a
//! [`SessionLock`] guard; requiring the guard to [`append`](SessionLog::append)
//! makes "no append without the lock" a compile-time invariant.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::fcntl::{Flock, FlockArg};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::error::{Error, Result};
use crate::record::{Record, RecordBody};

/// The environment variable that overrides the base data directory.
pub const DATA_DIR_ENV: &str = "WIFF_DATA_DIR";

/// The base data directory: the `WIFF_DATA_DIR` override if set, else the
/// platform data directory for wiff.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("", "", "wiff").ok_or(Error::NoDataDir)?;
    Ok(dirs.data_dir().to_path_buf())
}

/// The directory holding every project's sessions under `base`.
fn sessions_root(base: &Path) -> PathBuf {
    base.join("sessions")
}

/// A handle to a session log: its path, ULID, and next sequence number. It
/// holds no file descriptor and no lock; appending requires a [`SessionLock`].
#[derive(Debug, Clone)]
pub struct SessionLog {
    path: PathBuf,
    ulid: Ulid,
    next_seq: u64,
}

/// The exclusive lock on a session file and the sole handle through which it is
/// appended to. Constructed only when the lock is acquired; dropping it releases
/// the lock.
#[derive(Debug)]
pub struct SessionLock {
    file: Flock<std::fs::File>,
    path: PathBuf,
}

/// The outcome of attempting to acquire a session's lock.
#[derive(Debug)]
pub enum LockAttempt {
    /// Another process holds the lock.
    Contended,
    /// The lock was acquired.
    Acquired {
        /// The guard; drop it to release.
        lock: SessionLock,
        /// Whether our position still matches the file.
        sync: SyncState,
    },
}

/// Whether a freshly acquired lock finds the file in sync with our position.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncState {
    /// Our position matches the file; it is safe to append.
    Synced,
    /// The file advanced past our position since we last read it.
    Diverged {
        /// The next `seq` the file would assign.
        file_next_seq: u64,
    },
}

/// How to wait for a session's exclusive lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWait {
    /// Block until the lock becomes available. Suited to a one-shot process
    /// that has no event loop to keep responsive.
    Block,
    /// Fail immediately with [`Error::Locked`] when another process holds the
    /// lock. Suited to an interactive loop that must not stall.
    NonBlock,
}

impl SessionLog {
    /// The session's ULID.
    pub fn ulid(&self) -> Ulid {
        self.ulid
    }

    /// The session file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The sideband directory beside the session file (`<ULID>.d/`).
    pub fn sideband_dir(&self) -> PathBuf {
        self.path.with_extension("d")
    }

    /// Read the raw unified-diff text of captured version `number` from the
    /// sideband `vN.diff`.
    pub fn read_diff(&self, number: u32) -> Result<String> {
        let path = self.sideband_dir().join(format!("v{number}.diff"));
        std::fs::read_to_string(&path).map_err(|source| Error::io(&path, source))
    }

    /// The next sequence number this handle would assign.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Create a new session for `project` under `base`, writing its header
    /// through the returned lock. The header is built from the freshly assigned
    /// ULID via `build_header` so its embedded id matches the file name. The
    /// lock is taken before the header is written so no other process can claim
    /// the fresh file in the gap.
    pub fn create(
        base: &Path,
        project: &str,
        build_header: impl FnOnce(Ulid) -> RecordBody,
    ) -> Result<(Self, SessionLock)> {
        let ulid = Ulid::new();
        let dir = sessions_root(base).join(project);
        std::fs::create_dir_all(&dir).map_err(|source| Error::io(&dir, source))?;
        let path = dir.join(format!("{ulid}.jsonl"));
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|source| Error::io(&path, source))?;
        let locked = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, errno)| Error::io(&path, errno.into()))?;
        let mut lock = SessionLock {
            file: locked,
            path: path.clone(),
        };
        let mut log = Self {
            path,
            ulid,
            next_seq: 0,
        };
        log.append(&mut lock, build_header(ulid))?;
        Ok((log, lock))
    }

    /// Open an existing session for appending, positioning after its last
    /// record. Opens no descriptor and takes no lock: it reads the file lock-free
    /// to recover the next sequence number.
    pub fn open(path: &Path) -> Result<Self> {
        let ulid = ulid_from_path(path)?;
        let records = read_records(path)?;
        let next_seq = records.last().map(|record| record.seq + 1).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            ulid,
            next_seq,
        })
    }

    /// Append `body` as the next record through the held `lock`, flushing it to
    /// disk. Returns the assigned sequence number.
    pub fn append(&mut self, lock: &mut SessionLock, body: RecordBody) -> Result<u64> {
        debug_assert_eq!(
            lock.path, self.path,
            "the lock guard belongs to a different session"
        );
        let seq = self.next_seq;
        let record = Record {
            seq,
            at: OffsetDateTime::now_utc(),
            body,
        };
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        lock.file
            .write_all(line.as_bytes())
            .map_err(|source| Error::io(&self.path, source))?;
        lock.file
            .flush()
            .map_err(|source| Error::io(&self.path, source))?;
        self.next_seq += 1;
        Ok(seq)
    }

    /// Append `body` by taking the lock transiently. Fails without writing if
    /// the lock is contended or the file has diverged from our position, since
    /// appending then would duplicate a sequence number.
    pub fn append_locked(&mut self, body: RecordBody) -> Result<u64> {
        match self.lock()? {
            LockAttempt::Acquired {
                mut lock,
                sync: SyncState::Synced,
            } => self.append(&mut lock, body),
            LockAttempt::Acquired {
                sync: SyncState::Diverged { .. },
                ..
            } => Err(Error::Diverged(self.path.clone())),
            LockAttempt::Contended => Err(Error::Locked(self.path.clone())),
        }
    }

    /// Append `bodies` in order under a single lock acquisition, returning the
    /// assigned sequence numbers. The lock and the divergence check are taken
    /// once up front, so a contended or diverged file fails without writing any
    /// of the batch; a caller can then retry the whole batch without the risk
    /// that a partially written prefix duplicates on the next attempt.
    pub fn append_all_locked(&mut self, bodies: Vec<RecordBody>) -> Result<Vec<u64>> {
        match self.lock()? {
            LockAttempt::Acquired {
                mut lock,
                sync: SyncState::Synced,
            } => bodies
                .into_iter()
                .map(|body| self.append(&mut lock, body))
                .collect(),
            LockAttempt::Acquired {
                sync: SyncState::Diverged { .. },
                ..
            } => Err(Error::Diverged(self.path.clone())),
            LockAttempt::Contended => Err(Error::Locked(self.path.clone())),
        }
    }

    /// Take the file's exclusive lock without blocking, reporting whether it was
    /// acquired and, if so, whether our position still matches the file. The
    /// sync check reads the file after the lock is held, so its snapshot is
    /// stable.
    pub fn lock(&self) -> Result<LockAttempt> {
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|source| Error::io(&self.path, source))?;
        let locked = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(locked) => locked,
            Err((_, nix::errno::Errno::EAGAIN)) => {
                return Ok(LockAttempt::Contended);
            }
            Err((_, errno)) => return Err(Error::io(&self.path, errno.into())),
        };
        let file_next_seq = read_records(&self.path)?
            .last()
            .map(|record| record.seq + 1)
            .unwrap_or(0);
        let sync = if file_next_seq == self.next_seq {
            SyncState::Synced
        } else {
            SyncState::Diverged { file_next_seq }
        };
        Ok(LockAttempt::Acquired {
            lock: SessionLock {
                file: locked,
                path: self.path.clone(),
            },
            sync,
        })
    }

    /// Take the file's exclusive lock per `wait`, then resync our position to
    /// the file's current tail, returning the guard alongside the records read
    /// to recover it. Because the position is recovered while the lock is held,
    /// an append made through the returned guard cannot diverge. A caller that
    /// must inspect the log to build its record (the latest diff version, an
    /// existing comment) reads it from the returned records rather than parsing
    /// the file a second time.
    pub fn lock_and_sync(&mut self, wait: LockWait) -> Result<(SessionLock, Vec<Record>)> {
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|source| Error::io(&self.path, source))?;
        let arg = match wait {
            LockWait::Block => FlockArg::LockExclusive,
            LockWait::NonBlock => FlockArg::LockExclusiveNonblock,
        };
        let locked = match Flock::lock(file, arg) {
            Ok(locked) => locked,
            Err((_, nix::errno::Errno::EAGAIN)) => return Err(Error::Locked(self.path.clone())),
            Err((_, errno)) => return Err(Error::io(&self.path, errno.into())),
        };
        let records = read_records(&self.path)?;
        self.next_seq = records.last().map(|record| record.seq + 1).unwrap_or(0);
        let lock = SessionLock {
            file: locked,
            path: self.path.clone(),
        };
        Ok((lock, records))
    }
}

/// Read every well-formed record from a session file, in order. Reads lock-free
/// by path. Records whose type is unknown to this version are represented as
/// [`RecordBody::Unknown`] and retained in order so sequence numbers stay
/// meaningful.
pub fn read_records(path: &Path) -> Result<Vec<Record>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io(path, source)),
    };
    // A lock-free reader can catch an appender mid-write and see a final line
    // that is not yet newline-terminated. Every complete record ends in a
    // newline, so parse only through the last one and hold back any unterminated
    // remainder; a later read picks it up once the append finishes. A malformed
    // line that is newline-terminated is genuine corruption and still errors.
    // This relies on a serialized record never containing a raw newline of its
    // own: serde_json escapes newlines within strings, so the sole newline in a
    // record line is its terminator.
    let complete = match text.rfind('\n') {
        Some(last) => &text[..=last],
        None => "",
    };
    let mut records = Vec::new();
    for line in complete.lines() {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str(line)?);
    }
    Ok(records)
}

/// A cheap change detector for a session file, so a reader can pick up another
/// actor's appends without inotify. The log only ever grows, so its size paired
/// with its modification time is an exact change signal without reading or
/// hashing the file.
#[derive(Debug, Clone)]
pub struct SessionWatcher {
    path: PathBuf,
    seen: Option<Fingerprint>,
}

/// An opaque snapshot of a session file's state, compared to tell whether it has
/// advanced. A caller passes one from [`SessionWatcher::changed`] back to
/// [`SessionWatcher::acknowledge`] without inspecting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Fingerprint {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

impl SessionWatcher {
    /// Watch the session file at `path`, taking its current state as the
    /// baseline so only later appends register as a change.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let seen = fingerprint(&path);
        Self { path, seen }
    }

    /// The file's current fingerprint when it differs from the last acknowledged
    /// one, meaning another actor has appended since. `None` when it is
    /// unchanged, or when it cannot be stat-ed (a removed session simply stops
    /// registering changes).
    pub fn changed(&self) -> Option<Fingerprint> {
        let current = fingerprint(&self.path)?;
        (Some(current) != self.seen).then_some(current)
    }

    /// Record `fingerprint` as seen, so the change it stands for does not
    /// register again. Held apart from [`changed`](Self::changed) so a caller
    /// that fails to act on a change (a torn read of a line still being written)
    /// can leave it unacknowledged and retry on the next check.
    pub fn acknowledge(&mut self, fingerprint: Fingerprint) {
        self.seen = Some(fingerprint);
    }
}

/// Recover a session's ULID from its `<ULID>.jsonl` path.
fn ulid_from_path(path: &Path) -> Result<Ulid> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| Ulid::from_string(stem).ok())
        .ok_or_else(|| Error::NotASession {
            path: path.to_path_buf(),
            reason: "file name is not a ULID".to_string(),
        })
}

/// List every project bucket under `base`, sorted.
pub fn list_projects(base: &Path) -> Result<Vec<String>> {
    let root = sessions_root(base);
    let mut projects = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(projects),
        Err(source) => return Err(Error::io(&root, source)),
    };
    for entry in entries {
        let entry = entry.map_err(|source| Error::io(&root, source))?;
        if entry.path().is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            projects.push(name.to_string());
        }
    }
    projects.sort();
    Ok(projects)
}

/// List a project's session files, most recently modified first.
pub fn list_sessions(base: &Path, project: &str) -> Result<Vec<PathBuf>> {
    let dir = sessions_root(base).join(project);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io(&dir, source)),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::io(&dir, source))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .map_err(|source| Error::io(&path, source))?;
            sessions.push((modified, path));
        }
    }
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(sessions.into_iter().map(|(_, path)| path).collect())
}

/// The path a session with `ulid` would occupy under `project`. The file need
/// not exist; this only names where it lives.
pub fn session_file(base: &Path, project: &str, ulid: Ulid) -> PathBuf {
    sessions_root(base)
        .join(project)
        .join(format!("{ulid}.jsonl"))
}

/// The active session for a project: its most recently modified session file.
pub fn active_session(base: &Path, project: &str) -> Result<PathBuf> {
    list_sessions(base, project)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::NoSession(project.to_string()))
}

/// Remove a session: its `.jsonl` file and its `.d/` sideband directory.
pub fn remove_session(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::io(path, source)),
    }
    let sideband = path.with_extension("d");
    match std::fs::remove_dir_all(&sideband) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::io(&sideband, source)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use time::OffsetDateTime;
    use ulid::Ulid;

    use super::{SessionWatcher, read_records};
    use crate::error::Error;
    use crate::record::{Author, AuthorKind, CommentDelete, Record, RecordBody};

    /// A minimal well-formed record whose serialized line seeds the torn-read
    /// tests.
    fn sample_record() -> Record {
        Record {
            seq: 0,
            at: OffsetDateTime::from_unix_timestamp(0).expect("epoch"),
            body: RecordBody::CommentDelete(CommentDelete {
                id: Ulid::from_string("00000000000000000000000000").expect("ulid"),
                author: Author {
                    name: "reviewer".to_string(),
                    kind: AuthorKind::Human,
                },
            }),
        }
    }

    #[test]
    fn a_torn_final_line_is_held_back_until_the_append_completes() {
        // A complete newline-terminated record followed by a fragment of a
        // second, still-being-written line reads back as just the whole record;
        // the fragment is ignored until its newline arrives.
        let record = sample_record();
        let mut line = serde_json::to_string(&record).expect("serialize");
        line.push('\n');
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(line.as_bytes()).expect("whole record");
        file.write_all(br#"{"seq":1,"at":"2024"#)
            .expect("torn fragment");
        file.flush().expect("flush");

        let records = read_records(file.path()).expect("read");
        wince::assert_eq!(records, vec![record]);
    }

    #[test]
    fn a_malformed_interior_line_is_rejected_as_corruption() {
        // A garbage line that is newline-terminated, sitting between two whole
        // records, is not a torn tail; it is treated as corruption rather than
        // silently skipped.
        let record = sample_record();
        let mut line = serde_json::to_string(&record).expect("serialize");
        line.push('\n');
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(line.as_bytes()).expect("first record");
        file.write_all(b"not a record\n").expect("garbage line");
        file.write_all(line.as_bytes()).expect("second record");
        file.flush().expect("flush");

        let outcome = match read_records(file.path()) {
            Ok(records) => format!("ok with {} records", records.len()),
            Err(Error::Decode(_)) => "decode error".to_string(),
            Err(other) => format!("other error: {other}"),
        };
        wince::assert_eq!(outcome, "decode error".to_string());
    }

    #[test]
    fn an_empty_file_and_a_blank_only_file_both_read_as_no_records() {
        // An empty log and one holding only blank lines have no records; the
        // blank lines are skipped rather than parsed.
        let empty = tempfile::NamedTempFile::new().expect("temp file");
        empty.as_file().sync_all().expect("flush");

        let mut blank = tempfile::NamedTempFile::new().expect("temp file");
        blank.write_all(b"\n   \n\n").expect("blank lines");
        blank.flush().expect("flush");

        let empty_records = read_records(empty.path()).expect("read empty");
        let blank_records = read_records(blank.path()).expect("read blank");
        wince::assert_eq!((empty_records, blank_records), (Vec::new(), Vec::new()));
    }

    #[test]
    fn a_watcher_registers_an_append_once_until_it_is_acknowledged() {
        // A fresh watcher takes the file as its baseline and sees no change; an
        // append registers as changed and keeps registering until the change is
        // acknowledged, after which a further append registers anew.
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(b"one\n").expect("seed");
        file.flush().expect("flush");

        let mut watcher = SessionWatcher::new(file.path());
        wince::assert_eq!(watcher.changed().is_some(), false);

        file.write_all(b"two\n").expect("append");
        file.flush().expect("flush");
        let seen = watcher.changed().expect("the append registers");
        wince::assert_eq!(watcher.changed().is_some(), true);

        watcher.acknowledge(seen);
        wince::assert_eq!(watcher.changed().is_some(), false);

        file.write_all(b"three\n").expect("append");
        file.flush().expect("flush");
        wince::assert_eq!(watcher.changed().is_some(), true);
    }
}
