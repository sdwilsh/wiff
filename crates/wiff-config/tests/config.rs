#![allow(missing_docs)]

use wiff_config::{Appearance, Config, GeneratedRules, OnExit, ThemeConfig};
use wiff_core::record::{Author, AuthorKind};
use wiff_core::{AuthorDefaults, BaseRuleset, DEFAULT_BASE_REVISION_RULES};
use wiff_forge::{ForgeHost, ForgeTable};
use wiff_tui::keymap::KeymapOverrides;
use wiff_tui::{Action, Chord, Resolution, Scope};

fn chord(text: &str) -> Chord {
    text.parse().unwrap()
}

fn presses(text: &str) -> Vec<wiff_tui::KeyPress> {
    let Chord(presses) = text.parse().unwrap();
    presses
}

#[test]
fn a_full_config_parses_into_typed_settings() {
    let text = "\
on_exit = \"keep\"
display_context = 5
min_fold = 3
tab_width = 8
editor = \"vim +{line} {file}\"
wrap_lines = true
show_line_numbers = false
diff_mode = \"side_by_side\"
side_by_side_min_width = 160
nudge_to_detach = false
disable_default_keymap = false

[theme]
appearance = \"light\"
dark = \"Nord\"
light = \"GitHub\"
probe = false

[author]
human = \"wez\"
agent = \"opus\"

[section]
kotlin = ['^ *(fun|class) .*$']

[attachment]
kotlin = ['//', '/*']

[generated]
names = [\"gen/**/*.rs\", \"!Cargo.lock\"]
markers = [\"DO NOT EDIT\"]

[forge.\"git.example.org\"]
provider = \"forgejo\"
token_env = \"EXAMPLE_TOKEN\"

[keymap]
line_down = [\"j\", \"down\"]
quit = [\"q\", \"ctrl-c\"]
";
    let config = Config::parse(text).unwrap();

    let expected_keymap: KeymapOverrides = [
        (Action::LineDown, vec![chord("j"), chord("down")]),
        (Action::Quit, vec![chord("q"), chord("ctrl-c")]),
    ]
    .into_iter()
    .collect();
    wince::assert_eq!(
        config,
        Config {
            on_exit: OnExit::Keep,
            theme: ThemeConfig {
                appearance: Appearance::Light,
                dark: Some("Nord".parse().unwrap()),
                light: Some("GitHub".parse().unwrap()),
                probe: false,
            },
            display_context: 5,
            min_fold: 3,
            tab_width: 8,
            editor: Some("vim +{line} {file}".to_string()),
            wrap_lines: true,
            show_line_numbers: false,
            diff_mode: wiff_tui::render::DiffMode::SideBySide,
            side_by_side_min_width: 160,
            nudge_to_detach: false,
            author: AuthorDefaults {
                names: [
                    (AuthorKind::Human, "wez".to_string()),
                    (AuthorKind::Agent, "opus".to_string()),
                ]
                .into_iter()
                .collect(),
            },
            base_revision_rules: BaseRuleset::new(DEFAULT_BASE_REVISION_RULES),
            forge: ForgeTable::from([(
                "git.example.org".to_string(),
                ForgeHost {
                    provider: Some("forgejo".to_string()),
                    token_env: Some("EXAMPLE_TOKEN".to_string()),
                    ..ForgeHost::default()
                },
            )]),
            section: [(
                "kotlin".to_string(),
                vec![r"^ *(fun|class) .*$".to_string()],
            )]
            .into_iter()
            .collect(),
            attachment: [(
                "kotlin".to_string(),
                vec!["//".to_string(), "/*".to_string()],
            )]
            .into_iter()
            .collect(),
            generated: GeneratedRules {
                names: vec!["gen/**/*.rs".to_string(), "!Cargo.lock".to_string()],
                markers: vec!["DO NOT EDIT".to_string()],
            },
            disable_default_keymap: false,
            keymap: expected_keymap,
        }
    );
}

#[test]
fn an_empty_config_is_all_defaults() {
    let config = Config::parse("").unwrap();
    wince::assert_eq!(config, Config::default());
    wince::assert_eq!(config.on_exit, OnExit::Prompt);
    wince::assert_eq!(
        config.author.resolve(AuthorKind::Human),
        Author {
            name: std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
            kind: AuthorKind::Human,
        }
    );
    wince::assert_eq!(
        config.author.resolve(AuthorKind::Agent),
        Author {
            name: "assistant".to_string(),
            kind: AuthorKind::Agent,
        }
    );
}

#[test]
fn the_theme_defaults_to_automatic_with_probing() {
    let config = Config::parse("").unwrap();
    wince::assert_eq!(
        config.theme,
        ThemeConfig {
            appearance: Appearance::Auto,
            dark: None,
            light: None,
            probe: true,
        }
    );
    // The automatic appearance builds on the dark default and offers the light
    // default to switch to when a probe reads a light terminal.
    wince::assert_eq!(
        (
            config.theme.baseline_theme(),
            config.theme.probed_light_theme()
        ),
        ("wez", Some("GitHub"))
    );
}

#[test]
fn a_fixed_dark_appearance_pins_its_theme_and_never_probes() {
    // A dark appearance naming its own theme builds on that theme and, being
    // fixed, offers no light theme to switch to.
    let config =
        Config::parse("[theme]\nappearance = \"dark\"\ndark = \"Nord\"\nprobe = false\n").unwrap();
    wince::assert_eq!(
        config.theme,
        ThemeConfig {
            appearance: Appearance::Dark,
            dark: Some("Nord".parse().unwrap()),
            light: None,
            probe: false,
        }
    );
    wince::assert_eq!(
        (
            config.theme.baseline_theme(),
            config.theme.probed_light_theme()
        ),
        ("Nord", None)
    );
}

#[test]
fn a_bare_theme_string_is_rejected() {
    let error = Config::parse("theme = \"dark\"\n").unwrap_err();
    #[rustfmt::skip]
    wince::snapshot_display!(
        error,
        "could not parse config: TOML parse error at line 1, column 9\n",
        "  |\n",
        "1 | theme = \"dark\"\n",
        "  |         ^^^^^^\n",
        "invalid type: string \"dark\", expected struct ThemeConfig\n",
    );
}

#[test]
fn a_misspelled_theme_name_is_rejected_even_when_inactive() {
    // The light theme is validated at parse time regardless of the active
    // appearance, so a typo in a name that would only ever be reached through
    // the theme picker is still a hard error at load.
    let error = Config::parse("[theme]\nappearance = \"dark\"\nlight = \"Draclua\"\n").unwrap_err();
    #[rustfmt::skip]
    wince::snapshot_display!(
        error,
        "could not parse config: TOML parse error at line 3, column 9\n",
        "  |\n",
        "3 | light = \"Draclua\"\n",
        "  |         ^^^^^^^^^\n",
        "unknown theme \"Draclua\"; run `wiff themes` for the built-in names\n",
    );
}

#[test]
fn a_theme_table_defaults_the_probe_on() {
    let config = Config::parse("[theme]\nappearance = \"dark\"\n").unwrap();
    wince::assert_eq!(
        config.theme,
        ThemeConfig {
            appearance: Appearance::Dark,
            dark: None,
            light: None,
            probe: true,
        }
    );
    // A fixed appearance never probes, so it offers no light theme to switch to.
    wince::assert_eq!(
        (
            config.theme.baseline_theme(),
            config.theme.probed_light_theme()
        ),
        ("wez", None)
    );
}

#[test]
fn an_unknown_theme_field_is_rejected() {
    let error = Config::parse("[theme]\nsaturation = \"high\"\n").unwrap_err();
    #[rustfmt::skip]
    wince::snapshot_display!(
        error,
        "could not parse config: TOML parse error at line 2, column 1\n",
        "  |\n",
        "2 | saturation = \"high\"\n",
        "  | ^^^^^^^^^^\n",
        "unknown field `saturation`, expected one of `appearance`, `dark`, `light`, `probe`\n",
    );
}

#[test]
fn the_configured_keymap_overlays_the_defaults() {
    let config = Config::parse("[keymap]\nline_down = [\"R\"]\n").unwrap();
    let map = config.keymap().unwrap();
    // The override binds and the default "j" is gone, while untouched actions
    // keep their defaults.
    wince::assert_eq!(
        map.resolve(&presses("R"), Scope::REVIEW),
        Resolution::Action(Action::LineDown)
    );
    wince::assert_eq!(map.resolve(&presses("j"), Scope::REVIEW), Resolution::None);
    wince::assert_eq!(
        map.resolve(&presses("q"), Scope::REVIEW),
        Resolution::Action(Action::Quit)
    );
}

#[test]
fn an_unknown_field_is_rejected() {
    let error = Config::parse("wibble = true\n").unwrap_err();
    #[rustfmt::skip]
    wince::snapshot_display!(
        error,
        "could not parse config: TOML parse error at line 1, column 1\n",
        "  |\n",
        "1 | wibble = true\n",
        "  | ^^^^^^\n",
        "unknown field `wibble`, expected one of `on_exit`, `theme`, `display_context`, `min_fold`, `tab_width`, `editor`, `wrap_lines`, `show_line_numbers`, `diff_mode`, `side_by_side_min_width`, `nudge_to_detach`, `author`, `base_revision_rules`, `forge`, `section`, `attachment`, `generated`, `disable_default_keymap`, `keymap`\n",
    );
}

#[test]
fn an_invalid_chord_is_a_parse_error() {
    let error = Config::parse("[keymap]\nquit = [\"ctrl-nope\"]\n").unwrap_err();
    #[rustfmt::skip]
    wince::snapshot_display!(
        error,
        "could not parse config: TOML parse error at line 2, column 9\n",
        "  |\n",
        "2 | quit = [\"ctrl-nope\"]\n",
        "  |         ^^^^^^^^^^^\n",
        "unknown key \"nope\"\n",
    );
}
