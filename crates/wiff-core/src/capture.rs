//! Capturing a diff into a session.
//!
//! Creating a session and, later, refreshing it both come down to writing a
//! numbered diff version: the raw text goes to the sideband `vN.diff`, and a
//! [`DiffVersionRecord`] indexing its parsed structure is appended to the log,
//! referencing the sideband file by content hash.

use std::path::Path;

use ulid::Ulid;
use wiff_diff::parse::parse;

use crate::error::{Error, Result};
use crate::hash::SidebandHash;
use crate::identity::ProjectIdentity;
use crate::record::{
    Author, Description, DiffVersionRecord, FORMAT_VERSION, FileSummary, ForgeUrl, RecordBody,
    RevisionId, Seq, SessionHeader, VersionNumber,
};
use crate::session::{SessionLock, SessionLog};
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
    let initial = description
        .map(|(author, description)| crate::description::local_description(author, description))
        .into_iter()
        .collect();
    write_new_session(base, identity, cwd, None, Ulid::new(), captured, initial)
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
    session: Ulid,
    captured: &CapturedDiff,
    initial: Vec<RecordBody>,
) -> Result<SessionLog> {
    write_new_session(base, identity, cwd, Some(forge), session, captured, initial)
}

/// Write a new session's header, first diff version, and `initial` events under
/// one held lock, in that order, so a concurrent reader never sees a partial
/// session. `forge` records the bound pull request when the session is a forge
/// import.
fn write_new_session(
    base: &Path,
    identity: &ProjectIdentity,
    cwd: &Path,
    forge: Option<ForgeUrl>,
    session: Ulid,
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
        SessionLog::create_with_ulid(base, &identity.canonical, session, |ulid| {
            RecordBody::Session(SessionHeader {
                ulid,
                version: FORMAT_VERSION,
                project: identity.canonical.clone(),
                repo_root,
                cwd: cwd_text,
                source,
                forge,
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
