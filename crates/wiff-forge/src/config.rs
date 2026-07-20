//! Map hosts to the forge adapters that speak to them and the environment
//! variables their tokens come from, overlaying user config onto wiff's
//! built-in defaults for the public GitHub and Codeberg instances.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One host's row in the forge table: which adapter family speaks to it, its
/// API root, and the variables its token comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeHost {
    /// The provider family whose adapter speaks to this host: "github",
    /// "forgejo", and so on.
    pub provider: Option<String>,
    /// The API root URL, for an instance whose host is not the provider's
    /// public one.
    pub api_base: Option<String>,
    /// The environment variable whose value is the token.
    pub token_env: Option<String>,
    /// The environment variable whose value names a file holding the token.
    pub token_file_env: Option<String>,
}

impl ForgeHost {
    /// Overlay `self` onto `base`, taking each field from `self` where it is set
    /// and from `base` otherwise. A `None` field inherits, so a user entry can
    /// replace a built-in value but not clear one.
    fn overlay_onto(&self, base: &ForgeHost) -> ForgeHost {
        ForgeHost {
            provider: self.provider.clone().or_else(|| base.provider.clone()),
            api_base: self.api_base.clone().or_else(|| base.api_base.clone()),
            token_env: self.token_env.clone().or_else(|| base.token_env.clone()),
            token_file_env: self
                .token_file_env
                .clone()
                .or_else(|| base.token_file_env.clone()),
        }
    }
}

/// The user's forge configuration, keyed by host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgeTable(BTreeMap<String, ForgeHost>);

impl ForgeTable {
    /// Collect `entries` into a table, lowercasing each host so a lookup keyed
    /// by the lowercased host from [`ForgeUrl::host`](wiff_core::record::ForgeUrl::host)
    /// matches regardless of the case the user wrote.
    fn from_entries(entries: impl IntoIterator<Item = (String, ForgeHost)>) -> Self {
        ForgeTable(
            entries
                .into_iter()
                .map(|(host, row)| (host.to_ascii_lowercase(), row))
                .collect(),
        )
    }

    /// Looks `host` up, overlaying any user entry onto wiff's built-in default.
    /// Returns `None` when the host is neither built in nor configured.
    pub fn host(&self, host: &str) -> Option<ForgeHost> {
        let host = host.to_ascii_lowercase();
        match (self.0.get(&host), BUILT_IN_HOSTS.get(&host)) {
            (Some(user), Some(default)) => Some(user.overlay_onto(default)),
            (Some(user), None) => Some(user.clone()),
            (None, default) => default.cloned(),
        }
    }
}

impl<'de> Deserialize<'de> for ForgeTable {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let entries = BTreeMap::<String, ForgeHost>::deserialize(deserializer)?;
        Ok(ForgeTable::from_entries(entries))
    }
}

impl<const N: usize> From<[(String, ForgeHost); N]> for ForgeTable {
    fn from(entries: [(String, ForgeHost); N]) -> Self {
        ForgeTable::from_entries(entries)
    }
}

/// wiff's built-in rows for the public GitHub and Codeberg instances.
static BUILT_IN_HOSTS: LazyLock<BTreeMap<String, ForgeHost>> = LazyLock::new(|| {
    BTreeMap::from([
        (
            "github.com".to_string(),
            ForgeHost {
                provider: Some("github".to_string()),
                api_base: None,
                token_env: Some("GITHUB_TOKEN".to_string()),
                token_file_env: Some("GITHUB_TOKEN_FILE".to_string()),
            },
        ),
        (
            "codeberg.org".to_string(),
            ForgeHost {
                provider: Some("forgejo".to_string()),
                api_base: None,
                token_env: Some("CODEBERG_TOKEN".to_string()),
                token_file_env: Some("CODEBERG_TOKEN_FILE".to_string()),
            },
        ),
    ])
});

/// Command-line token overrides for a forge host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenOverride {
    /// A file whose trimmed contents are the token (`--forge-token-file`).
    pub token_file: Option<PathBuf>,
    /// The token given directly (`--forge-token`).
    pub token: Option<String>,
}

/// Resolves the forge token for `host`, trying the command-line overrides
/// before the host's configured environment variables. A named source that is
/// present but empty, or a file that cannot be read, is an error rather than a
/// fall-through to the next source. `env` looks a variable up, letting a caller
/// pass a closure over [`std::env::var`].
pub fn resolve_token(
    host: &ForgeHost,
    cli: &TokenOverride,
    env: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    if let Some(path) = &cli.token_file {
        return read_token_file(path);
    }
    if let Some(token) = &cli.token {
        return non_empty(token, || {
            "the forge token given with --forge-token is empty".to_string()
        });
    }
    if let Some(var) = &host.token_file_env
        && let Some(path) = env(var)
    {
        let path = path.trim();
        if path.is_empty() {
            bail!("the forge token file path in ${var} is empty");
        }
        return read_token_file(Path::new(path));
    }
    if let Some(var) = &host.token_env
        && let Some(value) = env(var)
    {
        return non_empty(&value, || format!("the forge token in ${var} is empty"));
    }
    bail!(
        "no forge token found; pass --forge-token-file or --forge-token, or set the host's token variable"
    );
}

/// Returns `value` trimmed, or the error `describe` builds when it is empty.
fn non_empty(value: &str, describe: impl FnOnce() -> String) -> Result<String> {
    let token = value.trim();
    if token.is_empty() {
        bail!(describe());
    }
    Ok(token.to_string())
}

/// Reads a token from `path`, trimming surrounding whitespace. A file that
/// cannot be read or is empty after trimming is an error.
fn read_token_file(path: &Path) -> Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading forge token from {}", path.display()))?;
    let token = contents.trim();
    if token.is_empty() {
        bail!("the forge token file {} is empty", path.display());
    }
    Ok(token.to_string())
}
