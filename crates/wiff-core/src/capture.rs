//! Capturing a diff into a session.
//!
//! Creating a session and, later, refreshing it both come down to writing a
//! numbered diff version: the raw text goes to the sideband `vN.diff`, and a
//! [`DiffVersionRecord`] indexing its parsed structure is appended to the log,
//! referencing the sideband file by content hash.

use std::path::Path;

use wiff_diff::parse::parse;

use crate::error::{Error, Result};
use crate::hash::SidebandHash;
use crate::identity::ProjectIdentity;
use crate::record::{
    Author, Description, DiffVersionRecord, FORMAT_VERSION, FileSummary, ForgeUrl, RecordBody,
    RevisionId, Seq, SessionHeader, VersionNumber,
};
use crate::refresh::{RefreshOutcome, refresh_session};
use crate::session::{LockWait, ProjectLock, SessionLock, SessionLog, session_with_source};
use crate::session_id::SessionId;
use crate::source::CapturedDiff;

/// Create a session for `identity` under `base`, capturing `captured` as its
/// first diff version (`v0`) and, when given, an initial description with its
/// author. Returns the open log positioned after the records written.
pub fn create_session(
    base: &Path,
    identity: &ProjectIdentity,
    cwd: &Path,
    captured: &CapturedDiff,
    description: Option<(Author, Description)>,
) -> Result<SessionLog> {
    let lock = ProjectLock::acquire(base, &identity.canonical, LockWait::Block)?;
    let initial = description
        .map(|(author, description)| crate::description::local_description(author, description))
        .into_iter()
        .collect();
    lock.write_new_session(base, identity, cwd, NewSession::fresh(), captured, initial)
}

/// The identity of a session being written: its id, and the pull request it
/// binds for a forge import.
struct NewSession {
    id: SessionId,
    forge: Option<ForgeUrl>,
}

impl NewSession {
    /// A freshly minted, unbound session, the shape a local `wiff new` creates.
    fn fresh() -> Self {
        Self {
            id: SessionId::new(),
            forge: None,
        }
    }

    /// A session under a caller-chosen id bound to the pull request at `forge`,
    /// the shape a forge import creates.
    fn bound(id: SessionId, forge: ForgeUrl) -> Self {
        Self {
            id,
            forge: Some(forge),
        }
    }
}

/// What an idempotent `wiff new --if-needed` did with the project's session for
/// a range: see [`reuse_or_create`].
pub enum IfNeeded {
    /// No session captured the range, so a fresh one was created.
    Created(SessionLog),
    /// A matching session already described the current state and was left as is.
    Unchanged(SessionLog),
    /// A matching session was stale and captured a new version in place.
    Refreshed(SessionLog, RefreshOutcome),
    /// No session captured the range and the capture is empty, so there was
    /// nothing to open a review from and none was created.
    NothingToReview,
}

/// Reuse the `identity` project's session capturing the same range as `captured`
/// rather than starting a parallel one: refresh it when the working copy has
/// moved on, report it unchanged when it already matches, and create a fresh
/// session only when none exists. `author` attributes a refresh's rebased
/// comments; `description` seeds a freshly created session.
///
/// An existing session is reused whatever `captured` holds, so a range whose
/// changes were reverted refreshes its session to the now-empty diff rather than
/// starting over. Only when no session exists and `captured` is empty is there
/// nothing to review, reported as [`IfNeeded::NothingToReview`]: a review cannot
/// be opened from an empty diff.
///
/// The project's creation lock is held across the "does a session for this range
/// exist?" check and the create that may follow, so a concurrent creator cannot
/// slip a session into the gap and be duplicated. It is released as soon as an
/// existing session is found, since that session's own lock guards the refresh.
pub fn reuse_or_create(
    base: &Path,
    identity: &ProjectIdentity,
    cwd: &Path,
    captured: &CapturedDiff,
    author: Author,
    description: Option<(Author, Description)>,
) -> Result<IfNeeded> {
    let lock = ProjectLock::acquire(base, &identity.canonical, LockWait::Block)?;
    if let Some(path) = session_with_source(base, &identity.canonical, &captured.source)? {
        drop(lock);
        let mut log = SessionLog::open(&path)?;
        return Ok(
            match refresh_session(&mut log, captured, author, LockWait::Block)? {
                Some(outcome) => IfNeeded::Refreshed(log, outcome),
                None => IfNeeded::Unchanged(log),
            },
        );
    }
    if captured.text.trim().is_empty() {
        return Ok(IfNeeded::NothingToReview);
    }
    let initial = description
        .map(|(author, description)| crate::description::local_description(author, description))
        .into_iter()
        .collect();
    let log =
        lock.write_new_session(base, identity, cwd, NewSession::fresh(), captured, initial)?;
    Ok(IfNeeded::Created(log))
}

/// Create a session bound to the forge pull request at `forge`, like
/// [`create_session`] but recording the binding and taking the caller-chosen
/// `session` id that the forge import has already keyed its fetched pins on.
/// `initial` is the imported metadata (the mirrored description, comments, and
/// reviews) to append after `v0`; it is written under the same held lock, so a
/// reader never observes the bound session without the metadata that was
/// imported alongside it.
///
/// The binding does not constrain `captured.source`: a repo-less import captures
/// a [`Forge`](crate::record::SourceKind::Forge) diff, while an in-repo import
/// binds a pull request to an [`Scm`](crate::record::SourceKind::Scm) source
/// that refreshes against the local repository.
pub fn create_forge_session(
    base: &Path,
    identity: &ProjectIdentity,
    cwd: &Path,
    forge: ForgeUrl,
    session: SessionId,
    captured: &CapturedDiff,
    initial: Vec<RecordBody>,
) -> Result<SessionLog> {
    let lock = ProjectLock::acquire(base, &identity.canonical, LockWait::Block)?;
    lock.write_new_session(
        base,
        identity,
        cwd,
        NewSession::bound(session, forge),
        captured,
        initial,
    )
}

impl ProjectLock {
    /// Write a new session's header, first diff version, and `initial` events
    /// under one held session lock, in that order, so a concurrent reader never
    /// sees a partial session. `forge` records the bound pull request when the
    /// session is a forge import. Taking `&self` proves the project's creation
    /// lock is held, so a caller that first checks whether a session already
    /// covers this range holds one lock across that check and the create.
    fn write_new_session(
        &self,
        base: &Path,
        identity: &ProjectIdentity,
        cwd: &Path,
        new: NewSession,
        captured: &CapturedDiff,
        initial: Vec<RecordBody>,
    ) -> Result<SessionLog> {
        let repo_root = identity
            .repo_root
            .as_ref()
            .map(|root| root.to_string_lossy().into_owned());
        let cwd_text = cwd.to_string_lossy().into_owned();
        let source = captured.source.clone();
        let (mut log, mut lock) =
            SessionLog::create_with_id(base, &identity.canonical, new.id, |id| {
                RecordBody::Session(SessionHeader {
                    id,
                    version: FORMAT_VERSION,
                    project: identity.canonical.clone(),
                    repo_root,
                    cwd: cwd_text,
                    source,
                    forge: new.forge,
                })
            })?;
        write_diff_version(
            &mut log,
            &mut lock,
            VersionNumber(0),
            &captured.text,
            captured.base_revision.clone(),
            captured.base_tip_relative,
            captured.head_revision.clone(),
        )?;
        for event in initial {
            log.append(&mut lock, event)?;
        }
        Ok(log)
    }
}

/// Write diff version `number` into `log` through the held `lock`: persist the
/// raw text to the sideband `vN.diff` and append its indexed
/// [`DiffVersionRecord`]. Returns the assigned sequence number.
///
/// A version is written once and never rewritten; a later change captures a new
/// numbered version rather than editing an existing one, and the record naming it
/// is appended only after the file is fully written. A reader that has observed
/// that record can therefore read `vN.diff` without the session lock: it sees a
/// complete diff, never a partial or superseded one (a removed session may find
/// it gone instead).
pub fn write_diff_version(
    log: &mut SessionLog,
    lock: &mut SessionLock,
    number: VersionNumber,
    diff_text: &str,
    base_revision: Option<RevisionId>,
    base_tip_relative: bool,
    head_revision: Option<RevisionId>,
) -> Result<Seq> {
    let diff = parse(diff_text)?;
    let dir = log.sideband_dir();
    std::fs::create_dir_all(&dir).map_err(|source| Error::io(&dir, source))?;
    let path = dir.join(format!("v{number}.diff"));
    std::fs::write(&path, diff_text).map_err(|source| Error::io(&path, source))?;
    let files = diff
        .files
        .iter()
        .map(|file| FileSummary {
            old_path: file.old_path.clone(),
            new_path: file.new_path.clone(),
            status: file.status,
            hunk_count: file.hunks.len() as u32,
        })
        .collect();
    let record = DiffVersionRecord {
        number,
        diff_hash: SidebandHash::of(diff_text.as_bytes()),
        base_revision,
        base_tip_relative,
        head_revision,
        files,
    };
    log.append(lock, RecordBody::DiffVersion(record))
}
