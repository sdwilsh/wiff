#![allow(missing_docs)]

use wiff_config::{Config, OnExit};
use wiff_core::AuthorDefaults;
use wiff_core::record::{Author, AuthorKind};
use wiff_tui::keymap::KeymapOverrides;
use wiff_tui::{Action, Chord, Resolution};

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
tab_width = 8
editor = \"vim +{line} {file}\"
wrap_lines = true
diff_mode = \"side_by_side\"
side_by_side_min_width = 160
nudge_to_detach = false
disable_default_keymap = false

[author]
human = \"wez\"
agent = \"opus\"

[section]
kotlin = ['^ *(fun|class) .*$']

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
            display_context: 5,
            tab_width: 8,
            editor: Some("vim +{line} {file}".to_string()),
            wrap_lines: true,
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
            section: [(
                "kotlin".to_string(),
                vec![r"^ *(fun|class) .*$".to_string()],
            )]
            .into_iter()
            .collect(),
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
fn the_configured_keymap_overlays_the_defaults() {
    let config = Config::parse("[keymap]\nline_down = [\"x\"]\n").unwrap();
    let map = config.keymap().unwrap();
    // The override binds and the default "j" is gone, while untouched actions
    // keep their defaults.
    wince::assert_eq!(
        map.resolve(&presses("x")),
        Resolution::Action(Action::LineDown)
    );
    wince::assert_eq!(map.resolve(&presses("j")), Resolution::None);
    wince::assert_eq!(map.resolve(&presses("q")), Resolution::Action(Action::Quit));
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
        "unknown field `wibble`, expected one of `on_exit`, `display_context`, `tab_width`, `editor`, `wrap_lines`, `diff_mode`, `side_by_side_min_width`, `nudge_to_detach`, `author`, `section`, `disable_default_keymap`, `keymap`\n",
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
