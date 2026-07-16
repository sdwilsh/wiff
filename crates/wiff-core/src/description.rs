//! Locally authored description revisions, appended to the session log. A
//! revision here binds no forge provenance; a mirrored revision that does is
//! built by the forge adapter.

use crate::error::Result;
use crate::record::{Author, Description, DescriptionRecord, RecordBody};
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
