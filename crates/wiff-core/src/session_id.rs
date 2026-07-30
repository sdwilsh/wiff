//! A session's identity, distinct from the ids wiff gives comments and other
//! objects.

use time::OffsetDateTime;

use crate::short_id::{ParseShortIdError, ShortId};

/// A session's identity: unique within a project directory, stable across the
/// runs of a review, and embedding the creation time so sessions sort in the
/// order they were made. Names the session's `{id}.jsonl` log file, so its text
/// form is filesystem- and shell-safe.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionId(ShortId);

impl SessionId {
    /// Mints a fresh session id.
    pub fn new() -> Self {
        Self(ShortId::new())
    }

    /// The instant this session was created, decoded from the id's timestamp.
    pub fn created_at(self) -> OffsetDateTime {
        self.0.minted_at()
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for SessionId {
    type Err = ParseShortIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self)
    }
}
