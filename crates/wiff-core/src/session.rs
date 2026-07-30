//! The persistent, append-only session log.
//!
//! A session is a JSON-lines file at `<data>/sessions/<project>/<ULID>.jsonl`,
//! with a sideband `<ULID>.d/` directory beside it. Reads are lock-free and go
//! by path, so they work even while another process is appending. Appends
//! require an exclusive [`flock`](nix::fcntl::Flock), held through a
//! [`SessionLock`] guard; requiring the guard to [`append`](SessionLog::append)
//! makes "no append without the lock" a compile-time invariant.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::fcntl::{Flock, FlockArg};

use crate::error::{Error, Result};
use crate::identity::ScmType;
use crate::record::{
    ForgeUrl, Record, RecordBody, ScmSource, Seq, SessionHeader, SourceKind, TipRule, VersionNumber,
};
use crate::session_id::SessionId;
use crate::short_id::ShortId;

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

/// A handle to a session log: its path, id, and next sequence number. It holds
/// no file descriptor and no lock; appending requires a [`SessionLock`].
#[derive(Debug, Clone)]
pub struct SessionLog {
    path: PathBuf,
    id: SessionId,
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

/// The exclusive lock that serializes session creation within one project.
///
/// A session file is named by a freshly minted ULID, so two processes creating
/// a session for the same range never collide on a path and would each write a
/// separate file. Holding this lock across the "is there already a session for
/// this range?" check and the create that follows makes that pair atomic: an
/// idempotent `wiff new --if-needed` cannot scan, miss a session a concurrent
/// creator is about to write, and duplicate it. Every creation path takes the
/// lock, so the check is exclusive against all of them. Dropping the guard
/// releases the lock.
#[derive(Debug)]
pub struct ProjectLock {
    // The lock lives in the open file description and is released when the file
    // is dropped; the field is held for that lifetime, never read.
    #[allow(dead_code)]
    file: Flock<std::fs::File>,
}

impl ProjectLock {
    /// Acquire the creation lock for `project` under `base` per `wait`, creating
    /// the project directory when it does not yet exist. Blocks until the lock
    /// is free under [`LockWait::Block`]; fails immediately with
    /// [`Error::Locked`] when it is held and `wait` is [`LockWait::NonBlock`].
    pub fn acquire(base: &Path, project: &str, wait: LockWait) -> Result<Self> {
        let dir = sessions_root(base).join(project);
        std::fs::create_dir_all(&dir).map_err(|source| Error::io(&dir, source))?;
        let path = dir.join(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| Error::io(&path, source))?;
        let arg = match wait {
            LockWait::Block => FlockArg::LockExclusive,
            LockWait::NonBlock => FlockArg::LockExclusiveNonblock,
        };
        let file = match Flock::lock(file, arg) {
            Ok(locked) => locked,
            Err((_, nix::errno::Errno::EAGAIN)) => return Err(Error::Locked(path)),
            Err((_, errno)) => return Err(Error::io(&path, errno.into())),
        };
        Ok(Self { file })
    }
}

impl SessionLog {
    /// The session's id.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The session file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The sideband directory beside the session file (`<id>.d/`).
    pub fn sideband_dir(&self) -> PathBuf {
        self.path.with_extension("d")
    }

    /// Read the raw unified-diff text of captured version `number` from the
    /// sideband `vN.diff`.
    pub fn read_diff(&self, number: VersionNumber) -> Result<String> {
        let path = self.sideband_dir().join(format!("v{number}.diff"));
        std::fs::read_to_string(&path).map_err(|source| Error::io(&path, source))
    }

    /// The next sequence number this handle would assign.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Create a new session for `project` under `base`, writing its header
    /// through the returned lock. The header is built from the freshly assigned
    /// id via `build_header` so its embedded id matches the file name. The lock
    /// is taken before the header is written so no other process can claim the
    /// fresh file in the gap.
    pub fn create(
        base: &Path,
        project: &str,
        build_header: impl FnOnce(SessionId) -> RecordBody,
    ) -> Result<(Self, SessionLock)> {
        Self::create_with_id(base, project, SessionId::new(), build_header)
    }

    /// Create a new session under a caller-chosen `id`, for a flow that keys
    /// state on the session id before the session file exists: a forge import
    /// writes its pins under the session's id, and must fetch into them before
    /// the captured diff that the session is created from is in hand. An id that
    /// already names a session is reported as [`Error::SessionExists`], which a
    /// caller reusing the existing session can match on.
    pub fn create_with_id(
        base: &Path,
        project: &str,
        id: SessionId,
        build_header: impl FnOnce(SessionId) -> RecordBody,
    ) -> Result<(Self, SessionLock)> {
        let dir = sessions_root(base).join(project);
        std::fs::create_dir_all(&dir).map_err(|source| Error::io(&dir, source))?;
        let path = dir.join(format!("{id}.jsonl"));
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|source| match source.kind() {
                std::io::ErrorKind::AlreadyExists => Error::SessionExists(id),
                _ => Error::io(&path, source),
            })?;
        let locked = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, errno)| Error::io(&path, errno.into()))?;
        let mut lock = SessionLock {
            file: locked,
            path: path.clone(),
        };
        let mut log = Self {
            path,
            id,
            next_seq: 0,
        };
        log.append(&mut lock, build_header(id))?;
        Ok((log, lock))
    }

    /// Open an existing session for appending, positioning after its last
    /// record. Opens no descriptor and takes no lock: it reads the file lock-free
    /// to recover the next sequence number.
    pub fn open(path: &Path) -> Result<Self> {
        let id = id_from_path(path)?;
        let records = read_records(path)?;
        let next_seq = records
            .last()
            .map(|record| record.seq.next().get())
            .unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            id,
            next_seq,
        })
    }

    /// Append `body` as the next record through the held `lock`, flushing it to
    /// disk. Returns the assigned sequence number.
    pub fn append(&mut self, lock: &mut SessionLock, body: RecordBody) -> Result<Seq> {
        debug_assert_eq!(
            lock.path, self.path,
            "the lock guard belongs to a different session"
        );
        let seq = Seq(self.next_seq);
        let record = Record {
            seq,
            at: crate::determinism::now(),
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
    pub fn append_locked(&mut self, body: RecordBody) -> Result<Seq> {
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
    pub fn append_all_locked(&mut self, bodies: Vec<RecordBody>) -> Result<Vec<Seq>> {
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
            .map(|record| record.seq.next().get())
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
        self.next_seq = records
            .last()
            .map(|record| record.seq.next().get())
            .unwrap_or(0);
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
    // not yet newline-terminated. That line is held back; a later read picks it
    // up once the append completes. An append writes the record and its
    // terminating newline in one call, so a present terminator means the whole
    // line reached disk. Everything up to the last newline is committed and must
    // parse: a newline-terminated line that fails to parse (including a blank
    // one) is genuine corruption, not a torn tail, and errors. The only
    // tolerance is the unterminated remainder after the last newline. This
    // relies on a serialized record never containing a raw newline of its own:
    // serde_json escapes newlines within strings, so the sole newline in a
    // record line is its terminator, and `str::lines` therefore splits on
    // record boundaries.
    //
    // Splitting at the last newline reads back the committed body without
    // scanning past it, so the length of the log ahead of the tail costs
    // nothing here.
    let committed = match text.rsplit_once('\n') {
        Some((body, _in_flight)) => body,
        // No newline at all: the whole file is an unterminated in-flight append.
        None => "",
    };
    let mut records = Vec::new();
    for line in committed.lines() {
        records.push(serde_json::from_str::<Record>(line).map_err(Error::Decode)?);
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

/// Recover a session's id from its `<id>.jsonl` path.
pub fn id_from_path(path: &Path) -> Result<SessionId> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse().ok())
        .ok_or_else(|| Error::NotASession {
            path: path.to_path_buf(),
            reason: "file name is not a session id".to_string(),
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

/// List a project's session files, most recent first. Recency is the later of
/// the session's creation time (decoded from the id in the file name) and the
/// file's modification time; a session created earlier but written to more
/// recently still sorts ahead. Equal recencies break on the file name; distinct
/// ids compare deterministically, so the order is total. Ids minted in the same
/// millisecond order by their monotonic tail, so this is a total order by mint.
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
            sessions.push((recency(&path, modified), path));
        }
    }
    sessions.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(sessions.into_iter().map(|(_, path)| path).collect())
}

/// The recency of a session file: the later of its creation time (decoded from
/// the id in the file name) and its last modification. An id that cannot be
/// decoded contributes only its modification time.
fn recency(path: &Path, modified: SystemTime) -> SystemTime {
    match id_from_path(path) {
        Ok(id) => {
            let created_ms =
                u64::try_from(id.created_at().unix_timestamp_nanos() / 1_000_000).unwrap_or(0);
            modified.max(UNIX_EPOCH + Duration::from_millis(created_ms))
        }
        Err(_) => modified,
    }
}

/// The path a session with `id` would occupy under `project`. The file need not
/// exist; this only names where it lives.
pub fn session_file(base: &Path, project: &str, id: SessionId) -> PathBuf {
    sessions_root(base)
        .join(project)
        .join(format!("{id}.jsonl"))
}

/// Resolve `query`, a full session id or any leading prefix of one, to the sole
/// session it names under `project`. Matching is case-insensitive and folds
/// Crockford's aliases, so a typed prefix need not reproduce an id's exact
/// spelling.
///
/// Fails when no session's id begins with `query` (including when `query` is not
/// valid id text), or when several do, in which case the error lists the
/// candidates so the caller can lengthen the prefix.
pub fn resolve_session_id(base: &Path, project: &str, query: &str) -> Result<SessionId> {
    let unknown = || Error::UnknownSessionId {
        project: project.to_string(),
        query: query.to_string(),
    };
    let canonical = ShortId::canonical_prefix(query).ok_or_else(unknown)?;
    let mut matches: Vec<SessionId> = list_sessions(base, project)?
        .iter()
        .filter_map(|path| id_from_path(path).ok())
        .filter(|id| id.has_prefix(&canonical))
        .collect();
    match matches.len() {
        0 => Err(unknown()),
        1 => Ok(matches.remove(0)),
        _ => Err(Error::AmbiguousSessionId {
            query: query.to_string(),
            matches,
        }),
    }
}

/// The session to act on in the current repository: the most recent session
/// whose tip (or, for a working-copy session, its branch hint) names the
/// checked-out branch, falling back to the most recent session of any kind when
/// none matches or the branch is unknown. With no repository context, or a
/// single session, this is simply the most recent session.
pub fn active_session(
    base: &Path,
    project: &str,
    repo_root: Option<&Path>,
    scm: Option<ScmType>,
) -> Result<PathBuf> {
    let sessions = list_sessions(base, project)?;
    let most_recent = sessions
        .first()
        .cloned()
        .ok_or_else(|| Error::NoSession(project.to_string()))?;
    // With only one candidate there is no branch to disambiguate, so skip the
    // scm probe the common single-session project would otherwise pay.
    if sessions.len() == 1 {
        return Ok(most_recent);
    }
    let current = match (repo_root, scm) {
        (Some(root), Some(scm)) => crate::source::current_branch(root, scm),
        _ => None,
    };
    let Some(current) = current else {
        return Ok(most_recent);
    };
    for path in &sessions {
        // A header we cannot read here (an in-flight create, or a damaged file)
        // is skipped rather than aborting the scan: an unrelated broken session
        // must not hide the healthy session for the checked-out branch.
        if let Ok(Some(header)) = read_header(path)
            && let SourceKind::Scm(source) = &header.source
            && source_names_branch(source, &current)
        {
            return Ok(path.clone());
        }
    }
    // No session names the branch, so the most recent is the answer. A damaged
    // header on that particular file is reported here, since the caller is about
    // to be pointed at it; a corrupt header elsewhere was already skipped above.
    read_header(&most_recent)?;
    Ok(most_recent)
}

/// The most recent session in `project` bound to the pull request at `url`, or
/// `None` when none is. A session is looked up by its binding rather than a
/// checked-out branch: the caller holds the pull request's URL and wants its
/// session whatever branch, if any, that session also tracks. A fresh review of
/// the same pull request starts a new session, so several may share a binding;
/// recency here is the later of a session's creation and its last write, so the
/// one worked in most recently wins.
///
/// A momentarily unreadable session errors rather than reading as "no binding",
/// since a caller deciding whether to resume or start a fresh review would
/// otherwise create a duplicate for a pull request that already has a session.
pub fn session_bound_to(base: &Path, project: &str, url: &ForgeUrl) -> Result<Option<PathBuf>> {
    for path in list_sessions(base, project)? {
        match read_header(&path) {
            Ok(Some(header)) if header.forge.as_ref() == Some(url) => return Ok(Some(path)),
            // A header for another pull request, or one not yet readable (an
            // in-flight create or a truncated file), is simply not this match.
            Ok(_) => {}
            // A first record that is not a header is a damaged session; skip it
            // rather than aborting, so an unrelated broken session cannot hide a
            // healthy one bound to this pull request.
            Err(Error::Decode(_) | Error::NotASession { .. }) => {}
            // An I/O failure is not evidence of a non-match, so it propagates.
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

/// The pull request the session at `path` is bound to, or `None` when it is
/// unbound or its header is not yet readable. Errors when the file is missing or
/// cannot be read, so a caller naming a session by ULID learns that it does not
/// exist rather than reading a missing file as "unbound".
pub fn session_binding(path: &Path) -> Result<Option<ForgeUrl>> {
    Ok(read_header(path)?.and_then(|header| header.forge))
}

/// The most recent session in `project` capturing the same range the same way as
/// `source` and not bound to a pull request, or `None` when none is. Used by
/// `wiff new --if-needed` to find the session a re-run would otherwise duplicate,
/// so it refreshes that session in place rather than starting a parallel one.
/// Only an scm source can match: it names a recipe to re-resolve and compare,
/// whereas a stdin source is a one-shot snapshot with no recipe to match and a
/// forge source belongs to `wiff forge pull`.
///
/// The match includes the branch the session was created on, since two branches
/// can share one working copy recipe (`merge-base(trunk)..working copy`) and telling
/// their sessions apart by that branch is what stops a run on one branch from
/// refreshing the other's session. A run on a different checked-out ref (a
/// rename, or a detached head) therefore starts a fresh session rather than
/// reusing the original.
///
/// A session whose header is in flight or corrupt is skipped rather than hiding
/// a healthy match; only an I/O failure aborts the scan. A caller holding the
/// project's creation lock across this check and the create it may follow leaves
/// no in-flight partial present here in the first place.
pub fn session_with_source(
    base: &Path,
    project: &str,
    source: &SourceKind,
) -> Result<Option<PathBuf>> {
    if !matches!(source, SourceKind::Scm(_)) {
        return Ok(None);
    }
    for path in list_sessions(base, project)? {
        match read_header(&path) {
            Ok(Some(header)) if header.forge.is_none() && &header.source == source => {
                return Ok(Some(path));
            }
            // A header for another range, one still unreadable (an in-flight
            // create or a truncated file), or a session bound to a pull request,
            // is simply not this match.
            Ok(_) => {}
            // A first record that is not a header is a damaged session; skip it
            // rather than aborting, so an unrelated broken session cannot hide a
            // healthy one capturing this range.
            Err(Error::Decode(_) | Error::NotASession { .. }) => {}
            // An I/O failure is not evidence of a non-match, so it propagates.
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

/// A session bound to a pull request, as reported by [`forge_bound_sessions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundSession {
    /// The session's log file.
    pub path: PathBuf,
    /// The session's identifier. Several sessions can bind one pull request (a
    /// fresh review forks a new one), so this is what tells them apart.
    pub id: SessionId,
    /// The pull request the session is bound to.
    pub url: ForgeUrl,
}

/// Every session in `project` bound to a pull request, most recent first. A
/// caller with no pull request in hand (a bare `wiff forge push`) uses this to
/// find the review to publish: one bound session is unambiguous, while several
/// mean the caller must name which by id.
///
/// A momentarily unreadable session errors rather than reading as "unbound", so
/// a caller does not act on a partial view of the bucket and, say, publish the
/// wrong review.
pub fn forge_bound_sessions(base: &Path, project: &str) -> Result<Vec<BoundSession>> {
    let mut bound = Vec::new();
    for path in list_sessions(base, project)? {
        match read_header(&path) {
            Ok(Some(header)) => {
                if let Some(url) = header.forge {
                    bound.push(BoundSession {
                        path,
                        id: header.id,
                        url,
                    });
                }
            }
            // A header not yet readable (an in-flight create or a truncated
            // file) is simply not a binding to report.
            Ok(None) => {}
            // A first record that is not a header is a damaged session; skip it
            // rather than aborting, so an unrelated broken session cannot hide a
            // healthy bound one.
            Err(Error::Decode(_) | Error::NotASession { .. }) => {}
            // An I/O failure is not evidence of no binding, so it propagates.
            Err(err) => return Err(err),
        }
    }
    Ok(bound)
}

/// Whether an scm source is the review of `branch` (a full ref name such as
/// `refs/heads/topic`): a ref tip that names it, or a working-copy tip created
/// on it. A `--change <branch>` session, whose committed ref tip holds the
/// branch's full name, therefore matches too. A pinned or change-id tip names no
/// branch and never matches here.
fn source_names_branch(source: &ScmSource, branch: &str) -> bool {
    match &source.tip {
        TipRule::Ref { name } => name == branch,
        TipRule::WorkingCopy | TipRule::Index => source.branch_hint.as_deref() == Some(branch),
        TipRule::ChangeId { .. } | TipRule::Pinned { .. } => false,
    }
}

/// Read a session's header, the first record of its file, reading only the first
/// line rather than the whole log. Returns `None` when the first line has no
/// terminator yet: a create still in flight, or an empty or truncated file.
/// Errors on an unreadable or corrupt file, or a first record that is not a
/// session header.
fn read_header(path: &Path) -> Result<Option<SessionHeader>> {
    let file = std::fs::File::open(path).map_err(|source| Error::io(path, source))?;
    let mut first = String::new();
    BufReader::new(file)
        .read_line(&mut first)
        .map_err(|source| Error::io(path, source))?;
    // A committed header ends in a newline. A line without one is not yet
    // readable: a create still in flight, or an empty or truncated file.
    let Some(line) = first.strip_suffix('\n') else {
        return Ok(None);
    };
    match serde_json::from_str::<Record>(line).map_err(Error::Decode)? {
        Record {
            body: RecordBody::Session(header),
            ..
        } => Ok(Some(header)),
        _ => Err(Error::NotASession {
            path: path.to_path_buf(),
            reason: "first record is not a session header".to_string(),
        }),
    }
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
    use crate::record::{
        Author, AuthorKind, CommentEvent, CommentEventKind, Record, RecordBody, Seq,
    };

    /// A minimal well-formed record whose serialized line seeds the torn-read
    /// tests.
    fn sample_record() -> Record {
        Record {
            seq: Seq(0),
            at: OffsetDateTime::from_unix_timestamp(0).expect("epoch"),
            body: RecordBody::CommentEvent(CommentEvent {
                id: Ulid::from_string("00000000000000000000000000").expect("ulid"),
                author: Author {
                    name: "reviewer".to_string(),
                    kind: AuthorKind::Human,
                },
                authored_at: None,
                origin: None,
                kind: CommentEventKind::Delete,
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
    fn a_newline_terminated_but_unparseable_final_line_is_rejected_as_corruption() {
        // A garbled final line that ends at a real newline is not a torn tail:
        // the terminator means the write completed, so it is genuine corruption
        // and errors rather than being silently dropped.
        let record = sample_record();
        let mut line = serde_json::to_string(&record).expect("serialize");
        line.push('\n');
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(line.as_bytes()).expect("whole record");
        file.write_all(b"{\"seq\":1,\"at\":\"tor\n")
            .expect("garbled terminated line");
        file.flush().expect("flush");

        let outcome = match read_records(file.path()) {
            Ok(records) => format!("ok with {} records", records.len()),
            Err(Error::Decode(_)) => "decode error".to_string(),
            Err(other) => format!("other error: {other}"),
        };
        wince::assert_eq!(outcome, "decode error".to_string());
    }

    #[test]
    fn an_empty_file_reads_as_no_records() {
        let empty = tempfile::NamedTempFile::new().expect("temp file");
        empty.as_file().sync_all().expect("flush");

        let records = read_records(empty.path()).expect("read empty");
        wince::assert_eq!(records, Vec::new());
    }

    #[test]
    fn a_file_of_blank_lines_is_rejected_as_corruption() {
        // A valid log holds only newline-terminated records; a blank line where
        // a record belongs is not a torn tail but corruption, so it errors
        // rather than being skipped.
        let mut blank = tempfile::NamedTempFile::new().expect("temp file");
        blank.write_all(b"\n   \n\n").expect("blank lines");
        blank.flush().expect("flush");

        let outcome = match read_records(blank.path()) {
            Ok(records) => format!("ok with {} records", records.len()),
            Err(Error::Decode(_)) => "decode error".to_string(),
            Err(other) => format!("other error: {other}"),
        };
        wince::assert_eq!(outcome, "decode error".to_string());
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
