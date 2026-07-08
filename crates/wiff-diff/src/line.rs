//! One-based line numbers.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

/// A 1-based line number. Being non-zero encodes the invariant that line
/// numbers start at 1, and lets `Option<LineNo>` reuse the zero niche for its
/// `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LineNo(NonZeroU32);

impl LineNo {
    /// Wrap `n`, returning `None` when `n` is zero.
    pub fn new(n: u32) -> Option<Self> {
        NonZeroU32::new(n).map(Self)
    }

    /// The underlying 1-based value.
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// The following line number.
    pub fn next(self) -> LineNo {
        LineNo(self.0.saturating_add(1))
    }
}

impl std::fmt::Display for LineNo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
