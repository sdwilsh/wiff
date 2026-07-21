//! Locally authored description revisions, appended to the session log. A
//! revision here binds no forge provenance; a mirrored revision that does is
//! built by the forge adapter.

use time::OffsetDateTime;

use crate::error::Result;
use crate::record::{
    Author, Description, DescriptionRecord, DescriptionSyncRecord, ExternalRef, RecordBody,
};
use crate::session::{LockWait, SessionLog};

/// Set `log`'s description to `description`, attributed to `author`, by
/// appending a revision. Because folding keeps only the latest revision, calling
/// this replaces any existing description.
pub fn set_description(
    log: &mut SessionLog,
    description: Description,
    author: Author,
    wait: LockWait,
) -> Result<()> {
    let (mut lock, _records) = log.lock_and_sync(wait)?;
    log.append(&mut lock, local_description(author, description))?;
    Ok(())
}

/// Build a [`DescriptionRecord`] for a locally-authored revision.
pub(crate) fn local_description(author: Author, description: Description) -> RecordBody {
    RecordBody::Description(DescriptionRecord {
        author,
        authored_at: None,
        origin: None,
        synced_marker: None,
        description,
    })
}

/// Build a record advancing the description's synced marker to `synced_marker`
/// after a push published the current content. It sets no new revision, so the
/// current content and its author stay put while the marker moves.
pub fn synced_description(synced_marker: String) -> RecordBody {
    RecordBody::DescriptionSync(DescriptionSyncRecord { synced_marker })
}

/// Build a [`DescriptionRecord`] mirroring a forge's description: bound to the
/// forge object `origin` at the forge's own `authored_at`, and marked with
/// `synced_marker` as the upstream content this revision reconciled with.
pub fn mirrored_description(
    author: Author,
    description: Description,
    origin: ExternalRef,
    authored_at: OffsetDateTime,
    synced_marker: String,
) -> RecordBody {
    RecordBody::Description(DescriptionRecord {
        author,
        authored_at: Some(authored_at),
        origin: Some(origin),
        synced_marker: Some(synced_marker),
        description,
    })
}
