//! The user's `config.toml`, deserialized into typed settings.
//!
//! This crate sits above `wiff-tui` and `wiff-core` so serde can parse the file
//! straight into their types: the keymap into [`wiff_tui`] actions and chords,
//! the author defaults into the struct `wiff-core` exports. Reading is a plain
//! deserialize; persisting individual settings back (a later feature for
//! remembered view options) will edit the document in place with `toml_edit` so
//! the user's formatting and comments survive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use wiff_core::AuthorDefaults;
use wiff_diff::DEFAULT_TAB_WIDTH;
use wiff_tui::keymap::Keymap;
use wiff_tui::render::{DEFAULT_DISPLAY_CONTEXT, DEFAULT_SIDE_BY_SIDE_MIN_WIDTH, DiffMode};
use wiff_tui::{KeymapError, KeymapOverrides};

/// The environment variable that overrides the config directory.
pub const CONFIG_DIR_ENV: &str = "WIFF_CONFIG_DIR";

/// What to do with a session when the review UI exits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExit {
    /// Ask whether to keep or remove the session.
    #[default]
    Prompt,
    /// Keep the session for later resumption.
    Keep,
    /// Remove the session.
    Remove,
}

/// The whole of the user's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How to resolve keep-or-remove when the UI exits.
    pub on_exit: OnExit,
    /// The unchanged lines kept on each side of a change when rendering; longer
    /// runs fold away. This is a display choice, independent of how much context
    /// the diff was captured with.
    pub display_context: usize,
    /// Columns per tab stop for diff display and comment editing.
    pub tab_width: usize,
    /// The editor command template for `open_in_editor`, with `{file}` and
    /// `{line}` placeholders; falls back to `$VISUAL`/`$EDITOR` when unset.
    pub editor: Option<String>,
    /// Whether diff content wraps to the viewport width instead of being clipped
    /// at the edge. The `toggle_wrap` action flips it within a session.
    pub wrap_lines: bool,
    /// The starting diff layout: one column, two columns, or auto (two once the
    /// terminal is wide enough). The `diff_mode_*` actions switch it within a
    /// session.
    pub diff_mode: DiffMode,
    /// The terminal width in columns at or above which `auto` mode chooses the
    /// side-by-side layout.
    pub side_by_side_min_width: usize,
    /// Whether pressing an arrow past the top or bottom of the open comment
    /// editor detaches it, floating it at a screen edge to free the cursor for
    /// navigating the diff. When false, only the `detach_editor` binding
    /// detaches.
    pub nudge_to_detach: bool,
    /// The default author identity for annotations.
    pub author: AuthorDefaults,
    /// Per-language patterns recognising the enclosing-definition line shown on a
    /// fold marker, keyed by language token. A language here replaces its
    /// built-in patterns; unlisted languages keep the built-ins. Patterns follow
    /// git's `userdiff` format, a leading `!` marking an exclusion.
    pub section: BTreeMap<String, Vec<String>>,
    /// Start from an empty keymap so only configured bindings take effect.
    pub disable_default_keymap: bool,
    /// Per-action chord overrides layered onto the built-in defaults.
    pub keymap: KeymapOverrides,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            on_exit: OnExit::default(),
            display_context: DEFAULT_DISPLAY_CONTEXT,
            tab_width: DEFAULT_TAB_WIDTH,
            editor: None,
            wrap_lines: true,
            diff_mode: DiffMode::default(),
            side_by_side_min_width: DEFAULT_SIDE_BY_SIDE_MIN_WIDTH,
            nudge_to_detach: true,
            author: AuthorDefaults::default(),
            section: BTreeMap::new(),
            disable_default_keymap: false,
            keymap: KeymapOverrides::default(),
        }
    }
}

impl Config {
    /// Parse a config from TOML text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(ConfigError::Parse)
    }

    /// Load the config from `path`, or the default config when the file does not
    /// exist.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                source: err,
            }),
        }
    }

    /// Load the config from the user's config directory, or the default config
    /// when no file is present.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&config_file()?)
    }

    /// Build the effective keymap: the built-in defaults (unless disabled)
    /// overlaid with the configured overrides.
    pub fn keymap(&self) -> Result<Keymap, KeymapError> {
        Keymap::resolve_config(&self.keymap, self.disable_default_keymap)
    }
}

/// The config directory: the `WIFF_CONFIG_DIR` override if set, else the
/// platform config directory for wiff.
pub fn config_dir() -> Result<PathBuf, ConfigError> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("", "", "wiff").ok_or(ConfigError::NoConfigDir)?;
    Ok(dirs.config_dir().to_path_buf())
}

/// The path to the config file within the config directory.
pub fn config_file() -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?.join("config.toml"))
}

/// An error loading the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The platform config directory could not be determined.
    #[error("could not determine the wiff config directory")]
    NoConfigDir,
    /// The config file could not be read.
    #[error("could not read config at {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The config file was not valid TOML or had invalid values.
    #[error("could not parse config: {0}")]
    Parse(#[source] toml::de::Error),
}
