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
    Author, Description, DiffVersionRecord, FORMAT_VERSION, FileSummary, RecordBody, RevisionId,
    Seq, SessionHeader, VersionNumber,
};
use crate::session::{SessionLock, SessionLog};
use crate::source::CapturedDiff;

/// Create a session for `identity` under `base`, capturing `captured` as its
/// first diff version (`v0`) and, when given, an initial description with its
/// author. The header, diff, and description are written under one held lock, in
/// that order, so a concurrent reader never sees a partial session. Returns the
/// open log positioned after the records written.
pub fn create_session(
    base: &Path,
    identity: &ProjectIdentity,
    cwd: &Path,
    captured: &CapturedDiff,
    description: Option<(Author, Description)>,
) -> Result<SessionLog> {
    let repo_root = identity
        .repo_root
        .as_ref()
        .map(|root| root.to_string_lossy().into_owned());
    let cwd_text = cwd.to_string_lossy().into_owned();
    let source = captured.source.clone();
    let (mut log, mut lock) = SessionLog::create(base, &identity.canonical, |ulid| {
        RecordBody::Session(SessionHeader {
            ulid,
            version: FORMAT_VERSION,
            project: identity.canonical.clone(),
            repo_root,
            cwd: cwd_text,
            source,
        })
    })?;
    write_diff_version(
        &mut log,
        &mut lock,
        VersionNumber(0),
        &captured.text,
        captured.base_revision.clone(),
        captured.head_revision.clone(),
    )?;
    if let Some((author, description)) = description {
        log.append(
            &mut lock,
            crate::description::local_description(author, description),
        )?;
    }
    Ok(log)
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
        head_revision,
        files,
    };
    log.append(lock, RecordBody::DiffVersion(record))
}
