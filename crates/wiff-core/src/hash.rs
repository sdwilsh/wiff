//! Content hashes for session sideband files.

use serde::{Deserialize, Serialize};

/// The blake3 content hash of a session sideband file (e.g. a `vN.diff`),
/// hex-encoded. A dedicated type keeps this hash domain distinct from any other
/// string or hash the code handles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SidebandHash(String);

impl SidebandHash {
    /// Compute the hash of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    /// The hex-encoded hash.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SidebandHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
