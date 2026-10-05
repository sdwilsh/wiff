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
use wiff_core::{AuthorDefaults, BaseRuleset, DEFAULT_BASE_REVISION_RULES};
use wiff_diff::highlight::{DEFAULT_DARK_THEME, DEFAULT_LIGHT_THEME};
use wiff_diff::{DEFAULT_TAB_WIDTH, ThemeName};
use wiff_forge::ForgeTable;
use wiff_tui::keymap::Keymap;
use wiff_tui::render::{
    DEFAULT_DISPLAY_CONTEXT, DEFAULT_MIN_FOLD, DEFAULT_SIDE_BY_SIDE_MIN_WIDTH, DiffMode,
};
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

/// Which appearance the review UI renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Follow the terminal background: render dark by default, switching to
    /// light on a terminal a probe reads as light.
    #[default]
    Auto,
    /// Always render the dark palette.
    Dark,
    /// Always render the light palette.
    Light,
}

/// How the review UI is colored: which theme to use and how it is chosen.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    /// Which appearance to render.
    pub appearance: Appearance,
    /// The syntax theme for the dark appearance, or `None` for the built-in
    /// dark default.
    pub dark: Option<ThemeName>,
    /// The syntax theme for the light appearance, or `None` for the built-in
    /// light default.
    pub light: Option<ThemeName>,
    /// Whether to query the terminal background at startup. Consulted only by
    /// the automatic appearance; a fixed appearance never probes.
    pub probe: bool,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            appearance: Appearance::Auto,
            dark: None,
            light: None,
            probe: true,
        }
    }
}

impl ThemeConfig {
    /// The syntax theme to build the UI with at startup: the dark theme for the
    /// automatic and dark appearances, the light theme for the light one.
    pub fn baseline_theme(&self) -> &str {
        match self.appearance {
            Appearance::Auto | Appearance::Dark => self.dark_theme(),
            Appearance::Light => self.light_theme(),
        }
    }

    /// The syntax theme a startup probe switches to when it reads a light
    /// terminal background, or `None` when the appearance is not automatic or
    /// probing is disabled.
    pub fn probed_light_theme(&self) -> Option<&str> {
        (self.appearance == Appearance::Auto && self.probe).then(|| self.light_theme())
    }

    fn dark_theme(&self) -> &str {
        self.dark
            .as_ref()
            .map_or(DEFAULT_DARK_THEME, ThemeName::as_str)
    }

    fn light_theme(&self) -> &str {
        self.light
            .as_ref()
            .map_or(DEFAULT_LIGHT_THEME, ThemeName::as_str)
    }
}

/// The whole of the user's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How to resolve keep-or-remove when the UI exits.
    pub on_exit: OnExit,
    /// How the review UI is colored.
    pub theme: ThemeConfig,
    /// The unchanged lines kept on each side of a change when rendering; longer
    /// runs fold away. This is a display choice, independent of how much context
    /// the diff was captured with.
    pub display_context: usize,
    /// The shortest run of unchanged lines that folds away; shorter runs stay
    /// expanded instead of collapsing behind a marker.
    pub min_fold: usize,
    /// Columns per tab stop for diff display and comment editing.
    pub tab_width: usize,
    /// The editor command template for `open_in_editor`, with `{file}` and
    /// `{line}` placeholders; falls back to `$VISUAL`/`$EDITOR` when unset.
    pub editor: Option<String>,
    /// Whether diff content wraps to the viewport width instead of being clipped
    /// at the edge. The `toggle_wrap` action flips it within a session.
    pub wrap_lines: bool,
    /// Whether the gutter shows line numbers on startup. The
    /// `toggle_line_numbers` action flips it within a session.
    pub show_line_numbers: bool,
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
    /// The default base ruleset for a whole-branch review (`wiff new
    /// --from-base`).
    pub base_revision_rules: BaseRuleset,
    /// Per-host forge settings: which adapter speaks to a host and where its
    /// token comes from.
    pub forge: ForgeTable,
    /// Per-language patterns recognising the enclosing-definition line shown on a
    /// fold marker, keyed by language token. A language here replaces its
    /// built-in patterns; unlisted languages keep the built-ins. Patterns follow
    /// git's `userdiff` format, a leading `!` marking an exclusion.
    pub section: BTreeMap<String, Vec<String>>,
    /// Per-language line prefixes marking a comment or attribute line that
    /// leads into the definition below it, keyed by language token. A language
    /// here replaces its built-in prefixes. Unlisted languages keep the
    /// built-ins. A blank line leads into a definition in every language and
    /// need not be listed.
    pub attachment: BTreeMap<String, Vec<String>>,
    /// Extra globs and markers that mark a file machine-generated, layered onto
    /// the built-in sets. A recognised file shows a `[generated]` badge and folds
    /// to its header by default.
    pub generated: GeneratedRules,
    /// Start from an empty keymap so only configured bindings take effect.
    pub disable_default_keymap: bool,
    /// Per-action chord overrides layered onto the built-in defaults.
    pub keymap: KeymapOverrides,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            on_exit: OnExit::default(),
            theme: ThemeConfig::default(),
            display_context: DEFAULT_DISPLAY_CONTEXT,
            min_fold: DEFAULT_MIN_FOLD,
            tab_width: DEFAULT_TAB_WIDTH,
            editor: None,
            wrap_lines: true,
            show_line_numbers: true,
            diff_mode: DiffMode::default(),
            side_by_side_min_width: DEFAULT_SIDE_BY_SIDE_MIN_WIDTH,
            nudge_to_detach: true,
            author: AuthorDefaults::default(),
            base_revision_rules: BaseRuleset::new(DEFAULT_BASE_REVISION_RULES),
            forge: ForgeTable::default(),
            section: BTreeMap::new(),
            attachment: BTreeMap::new(),
            generated: GeneratedRules::default(),
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

/// Extra generated-file globs and markers layered onto wiff's built-in sets. A
/// plain entry adds to a set; an entry led by `!` drops a built-in equal to its
/// remainder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeneratedRules {
    /// Path globs whose match marks a file generated, checked against the
    /// basename, or the whole path when the glob contains a `/`. `**` matches
    /// across path separators, `*` within a segment, `?` a single character, and
    /// `[...]` a character set (with `a-z` ranges and a leading `!` negating it).
    pub names: Vec<String>,
    /// Strings whose appearance near the top of a file marks it generated. A
    /// marker is only found when the file's opening lines reach the diff, so name
    /// globs are the reliable choice for a file changed only far from its head.
    pub markers: Vec<String>,
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
