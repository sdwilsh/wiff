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
    DiffVersionRecord, FORMAT_VERSION, FileSummary, RecordBody, Seq, SessionHeader, VersionNumber,
};
use crate::session::{SessionLock, SessionLog};
use crate::source::CapturedDiff;

/// Create a session for `identity` under `base`, capturing `captured` as its
/// first diff version (`v0`). Returns the open log positioned after the version
/// record.
pub fn create_session(
    base: &Path,
    identity: &ProjectIdentity,
    cwd: &Path,
    captured: &CapturedDiff,
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
    write_diff_version(&mut log, &mut lock, VersionNumber(0), &captured.text)?;
    Ok(log)
}

/// Write diff version `number` into `log` through the held `lock`: persist the
/// raw text to the sideband `vN.diff` and append its indexed
/// [`DiffVersionRecord`]. Returns the assigned sequence number.
pub fn write_diff_version(
    log: &mut SessionLog,
    lock: &mut SessionLock,
    number: VersionNumber,
    diff_text: &str,
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
        files,
    };
    log.append(lock, RecordBody::DiffVersion(record))
}
