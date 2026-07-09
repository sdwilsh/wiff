//! Configuration types wiff-core exports for the file-level config model to
//! embed, so a downstream config crate can deserialize them directly.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::record::{Author, AuthorKind};

/// The name an unconfigured agent annotates under, matching the "assistant"
/// role LLM protocols use.
const DEFAULT_AGENT_NAME: &str = "assistant";

/// The default author names per kind, as configured by the user.
///
/// The same config is read by a human in the TUI and by an agent through the
/// CLI, so each supplies its own name here rather than the file committing to
/// one identity. The caller selects a name by the kind it is acting as; an
/// unconfigured human falls back to `$USER` and an unconfigured agent to
/// "assistant". CLI flags override whatever is resolved here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthorDefaults {
    /// The configured name for each author kind.
    pub names: BTreeMap<AuthorKind, String>,
}

impl AuthorDefaults {
    /// Resolve the author to annotate as when acting as `kind`, using the
    /// configured name when present and the per-kind fallback otherwise.
    pub fn resolve(&self, kind: AuthorKind) -> Author {
        let name = self
            .names
            .get(&kind)
            .cloned()
            .unwrap_or_else(|| default_name(kind));
        Author { name, kind }
    }
}

/// The fallback name for `kind` when the user has not configured one.
fn default_name(kind: AuthorKind) -> String {
    match kind {
        AuthorKind::Human => std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
        AuthorKind::Agent => DEFAULT_AGENT_NAME.to_string(),
    }
}
