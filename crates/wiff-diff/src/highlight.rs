//! Syntax highlighting of file content with syntect.
//!
//! Highlighting a diff means highlighting the file content it shows, so a
//! [`Highlighter`] reconstructs one side of a file and colors its known lines,
//! returning each line's colored spans keyed by line number for the renderer to
//! place against the diff. The spans are backend-neutral ([`Rgb`] plus bold and
//! friends) rather than syntect or ratatui types, so nothing downstream takes a
//! syntect dependency and the output is straightforward to assert.

use std::collections::BTreeMap;

use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter as ThemeHighlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};

use crate::line::LineNo;
use crate::model::{FileDiff, Side};
use crate::reconstitute::{ReconLine, reconstitute};

/// The default theme for a dark terminal.
pub const DEFAULT_DARK_THEME: &str = "base16-ocean.dark";

/// The default theme for a light terminal.
pub const DEFAULT_LIGHT_THEME: &str = "InspiredGitHub";

/// The chrome-relevant colors of a syntax theme, taken from its editor settings
/// and reduced to [`Rgb`] so a consumer can derive a matching interface palette
/// without depending on syntect. A setting a theme leaves unspecified is `None`,
/// leaving the fallback to the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeChrome {
    /// The editor background.
    pub background: Option<Rgb>,
    /// The default text color.
    pub foreground: Option<Rgb>,
    /// The gutter text color, where line numbers are drawn.
    pub gutter_foreground: Option<Rgb>,
    /// The background of selected text.
    pub selection: Option<Rgb>,
    /// The background of a search match.
    pub find_highlight: Option<Rgb>,
}

/// The names of the built-in syntax themes, sorted, for listing the choices a
/// reviewer can switch between.
pub fn theme_names() -> Vec<String> {
    let mut names: Vec<String> = ThemeSet::load_defaults().themes.into_keys().collect();
    names.sort();
    names
}

/// The chrome colors of the built-in theme `name`, or `None` when no such theme
/// is bundled.
pub fn theme_chrome(name: &str) -> Option<ThemeChrome> {
    let themes = ThemeSet::load_defaults();
    let settings = &themes.themes.get(name)?.settings;
    Some(ThemeChrome {
        background: settings.background.map(rgb_of),
        foreground: settings.foreground.map(rgb_of),
        gutter_foreground: settings.gutter_foreground.map(rgb_of),
        selection: settings.selection.map(rgb_of),
        find_highlight: settings.find_highlight.map(rgb_of),
    })
}

/// Drop a syntect color's alpha to keep the opaque [`Rgb`] the renderer uses.
fn rgb_of(c: syntect::highlighting::Color) -> Rgb {
    Rgb {
        r: c.r,
        g: c.g,
        b: c.b,
    }
}

/// The markdown fence language token for a path's extension, or `None` when the
/// extension is unknown.
///
/// This is the single place the extension-to-language mapping lives so the diff
/// and markdown renderers agree. syntect cannot supply these tokens: its
/// default set lacks some languages entirely (TOML, TypeScript) and names
/// others in ways no fence understands ("Bourne Again Shell (bash)").
pub fn fence_language(path: &str) -> Option<&'static str> {
    const LANG_BY_EXT: &[(&str, &str)] = &[
        ("c", "c"),
        ("cpp", "cpp"),
        ("go", "go"),
        ("js", "javascript"),
        ("json", "json"),
        ("md", "markdown"),
        ("py", "python"),
        ("rb", "ruby"),
        ("rs", "rust"),
        ("sh", "bash"),
        ("toml", "toml"),
        ("ts", "typescript"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
    ];
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    LANG_BY_EXT
        .iter()
        .find(|(candidate, _)| *candidate == ext)
        .map(|(_, lang)| *lang)
}

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    /// The red channel.
    pub r: u8,
    /// The green channel.
    pub g: u8,
    /// The blue channel.
    pub b: u8,
}

/// The visual style of a highlighted span: a foreground color and font flags.
/// The background is left to the renderer, which tints whole lines by diff role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// The foreground color.
    pub fg: Rgb,
    /// Whether the span is bold.
    pub bold: bool,
    /// Whether the span is italic.
    pub italic: bool,
    /// Whether the span is underlined.
    pub underline: bool,
}

/// A run of text sharing one [`Style`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    /// The span's text, without a trailing newline.
    pub text: String,
    /// The span's style.
    pub style: Style,
}

/// A single line's colored spans, in order.
pub type HighlightedLine = Vec<StyledSpan>;

/// One line's parsed scope operations, ready to be colored by any theme without
/// parsing it again. The text keeps the trailing newline the parser saw so
/// coloring closes line-scoped constructs exactly as parsing opened them.
#[derive(Debug, Clone)]
struct ParsedLine {
    lineno: LineNo,
    text: String,
    ops: Vec<(usize, ScopeStackOp)>,
}

/// One side of a file parsed into scope operations, the costly part of
/// highlighting done once so a later theme change only recolors. The known
/// lines are grouped into the contiguous runs between reconstruction gaps, since
/// coloring restarts its scope state at each gap just as parsing did.
#[derive(Debug, Clone, Default)]
pub struct ParsedSide {
    runs: Vec<Vec<ParsedLine>>,
}

/// An error building a highlighter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HighlightError {
    /// The named theme is not among the built-in themes.
    #[error("unknown theme {name:?}")]
    UnknownTheme {
        /// The theme name that was not found.
        name: String,
    },
}

/// A reusable syntax highlighter over the built-in syntaxes and a chosen theme.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    /// Build a highlighter using the named built-in theme.
    pub fn with_theme(name: &str) -> Result<Self, HighlightError> {
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove(name)
            .ok_or_else(|| HighlightError::UnknownTheme {
                name: name.to_string(),
            })?;
        Ok(Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme,
        })
    }

    /// Recolor to the named built-in theme, keeping the loaded syntaxes so a
    /// caller can recolor a cached parse instead of highlighting the diff
    /// afresh. Looking the theme up still builds the default theme set, so this
    /// is not free end to end; what it saves is the syntax parse, not the theme
    /// load. On an unknown theme the highlighter is left unchanged.
    pub fn set_theme(&mut self, name: &str) -> Result<(), HighlightError> {
        let mut themes = ThemeSet::load_defaults();
        self.theme = themes
            .themes
            .remove(name)
            .ok_or_else(|| HighlightError::UnknownTheme {
                name: name.to_string(),
            })?;
        Ok(())
    }

    /// Highlight the known lines of `side` of `file`, keyed by line number.
    ///
    /// The file's path selects the syntax; gaps in the reconstruction break the
    /// highlighter's state, since the omitted lines could carry multi-line
    /// constructs whose effect cannot be known.
    pub fn highlight_side(&self, file: &FileDiff, side: Side) -> BTreeMap<LineNo, HighlightedLine> {
        self.color_side(&self.parse_side(file, side))
    }

    /// Parse the known lines of `side` of `file` into their scope operations,
    /// the theme-independent, costly part of highlighting, so a later theme
    /// change recolors without parsing again. The file's path selects the
    /// syntax; a reconstruction gap ends the current run and restarts parsing,
    /// since the omitted lines could carry multi-line constructs whose effect
    /// cannot be known.
    pub fn parse_side(&self, file: &FileDiff, side: Side) -> ParsedSide {
        let syntax = self.syntax_for(file.display_path());
        let mut state = ParseState::new(syntax);
        let mut runs = Vec::new();
        let mut run = Vec::new();
        for line in reconstitute(file, side) {
            match line {
                ReconLine::Known { lineno, text } => {
                    // syntect's newline-aware syntaxes expect a trailing newline
                    // to close line-scoped constructs; coloring later trims it
                    // back off, so it never appears in the returned spans.
                    let text = format!("{text}\n");
                    let ops = state.parse_line(&text, &self.syntaxes).unwrap_or_default();
                    run.push(ParsedLine { lineno, text, ops });
                }
                ReconLine::Gap { .. } => {
                    if !run.is_empty() {
                        runs.push(std::mem::take(&mut run));
                    }
                    state = ParseState::new(syntax);
                }
            }
        }
        if !run.is_empty() {
            runs.push(run);
        }
        ParsedSide { runs }
    }

    /// Color a `parsed` side with the current theme, keyed by line number. This
    /// is the cheap part of highlighting: a theme change only replays the cached
    /// scope operations through the new theme. Each run restarts the scope state
    /// its parse began with.
    pub fn color_side(&self, parsed: &ParsedSide) -> BTreeMap<LineNo, HighlightedLine> {
        let highlighter = ThemeHighlighter::new(&self.theme);
        let mut out = BTreeMap::new();
        for run in &parsed.runs {
            let mut state = HighlightState::new(&highlighter, ScopeStack::new());
            for line in run {
                let spans = HighlightIterator::new(&mut state, &line.ops, &line.text, &highlighter)
                    .map(|(style, piece)| StyledSpan {
                        text: piece.trim_end_matches('\n').to_string(),
                        style: convert_style(style),
                    })
                    .filter(|span| !span.text.is_empty())
                    .collect();
                out.insert(line.lineno, spans);
            }
        }
        out
    }

    /// The syntax for a file path, falling back to plain text when none matches.
    fn syntax_for(&self, path: &str) -> &SyntaxReference {
        std::path::Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text())
    }
}

/// Convert a syntect style into the backend-neutral [`Style`].
fn convert_style(style: syntect::highlighting::Style) -> Style {
    Style {
        fg: Rgb {
            r: style.foreground.r,
            g: style.foreground.g,
            b: style.foreground.b,
        },
        bold: style.font_style.contains(FontStyle::BOLD),
        italic: style.font_style.contains(FontStyle::ITALIC),
        underline: style.font_style.contains(FontStyle::UNDERLINE),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{HighlightError, HighlightedLine, Highlighter, Rgb, Style, StyledSpan};
    use crate::line::LineNo;
    use crate::model::{DiffLine, FileDiff, FileStatus, Hunk, LineKind, Side};

    /// A fixed theme for the tests, so changing the shipped default does not
    /// churn these fixtures.
    const TEST_THEME: &str = "base16-ocean.dark";

    fn ln(n: u32) -> LineNo {
        LineNo::new(n).expect("nonzero line number")
    }

    /// Parse a `#RRGGBB` color.
    fn rgb(hex: &str) -> Rgb {
        let n = u32::from_str_radix(hex.strip_prefix('#').expect("leading #"), 16)
            .expect("six hex digits");
        Rgb {
            r: (n >> 16) as u8,
            g: (n >> 8) as u8,
            b: n as u8,
        }
    }

    /// A plainly colored span, the shape every span in these fixtures takes.
    fn span(text: &str, hex: &str) -> StyledSpan {
        StyledSpan {
            text: text.to_string(),
            style: Style {
                fg: rgb(hex),
                bold: false,
                italic: false,
                underline: false,
            },
        }
    }

    /// A one-hunk file diff over `lines`, each `(kind, text, lineno)`.
    fn file(path: &str, lines: &[(LineKind, &str, u32)]) -> FileDiff {
        let diff_lines = lines
            .iter()
            .map(|(kind, text, n)| {
                let on_before = matches!(kind, LineKind::Context | LineKind::Removed);
                let on_after = matches!(kind, LineKind::Context | LineKind::Added);
                DiffLine {
                    kind: *kind,
                    text: (*text).to_string(),
                    old_lineno: on_before.then(|| ln(*n)),
                    new_lineno: on_after.then(|| ln(*n)),
                }
            })
            .collect();
        FileDiff {
            old_path: path.to_string(),
            new_path: path.to_string(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: lines.len() as u32,
                new_start: 1,
                new_len: lines.len() as u32,
                section: None,
                lines: diff_lines,
            }],
        }
    }

    #[test]
    fn colors_a_files_after_side_by_syntax() {
        let file = file(
            "src/lib.rs",
            &[
                (LineKind::Context, "let x = 1;", 1),
                (LineKind::Added, "// note", 2),
            ],
        );
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();

        let expected: BTreeMap<LineNo, HighlightedLine> = [
            (
                ln(1),
                vec![
                    span("let", "#b48ead"),
                    span(" x ", "#c0c5ce"),
                    span("=", "#c0c5ce"),
                    span(" ", "#c0c5ce"),
                    span("1", "#d08770"),
                    span(";", "#c0c5ce"),
                ],
            ),
            (ln(2), vec![span("//", "#65737e"), span(" note", "#65737e")]),
        ]
        .into_iter()
        .collect();
        k9::assert_equal!(highlighter.highlight_side(&file, Side::After), expected);
    }

    #[test]
    fn highlights_the_before_side_including_removed_lines() {
        let file = file(
            "src/lib.rs",
            &[
                (LineKind::Context, "let x = 1;", 1),
                (LineKind::Removed, "let y = 2;", 2),
            ],
        );
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();

        let expected: BTreeMap<LineNo, HighlightedLine> = [
            (
                ln(1),
                vec![
                    span("let", "#b48ead"),
                    span(" x ", "#c0c5ce"),
                    span("=", "#c0c5ce"),
                    span(" ", "#c0c5ce"),
                    span("1", "#d08770"),
                    span(";", "#c0c5ce"),
                ],
            ),
            (
                ln(2),
                vec![
                    span("let", "#b48ead"),
                    span(" y ", "#c0c5ce"),
                    span("=", "#c0c5ce"),
                    span(" ", "#c0c5ce"),
                    span("2", "#d08770"),
                    span(";", "#c0c5ce"),
                ],
            ),
        ]
        .into_iter()
        .collect();
        k9::assert_equal!(highlighter.highlight_side(&file, Side::Before), expected);
    }

    #[test]
    fn an_unknown_extension_falls_back_to_one_plain_span() {
        let file = file("notes.unknownext", &[(LineKind::Context, "hello world", 1)]);
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();

        let expected: BTreeMap<LineNo, HighlightedLine> =
            [(ln(1), vec![span("hello world", "#c0c5ce")])]
                .into_iter()
                .collect();
        k9::assert_equal!(highlighter.highlight_side(&file, Side::After), expected);
    }

    #[test]
    fn fence_language_maps_known_extensions_and_ignores_the_rest() {
        use super::fence_language;
        k9::assert_equal!(fence_language("src/lib.rs"), Some("rust"));
        k9::assert_equal!(fence_language("deploy.sh"), Some("bash"));
        k9::assert_equal!(fence_language("conf.yml"), Some("yaml"));
        k9::assert_equal!(fence_language("Cargo.toml"), Some("toml"));
        k9::assert_equal!(fence_language("notes.unknownext"), None);
        k9::assert_equal!(fence_language("Makefile"), None);
    }

    #[test]
    fn an_unknown_theme_is_an_error() {
        k9::assert_equal!(
            Highlighter::with_theme("no-such-theme").err(),
            Some(HighlightError::UnknownTheme {
                name: "no-such-theme".to_string(),
            })
        );
    }

    #[test]
    fn a_theme_switch_recolors_a_cached_parse_to_match_a_fresh_highlight() {
        // The whole point of splitting parse from color: a parse taken under one
        // theme, recolored after switching to another, must match highlighting
        // the file fresh under that other theme. This guards the cheap
        // theme-switch path against drifting from the full highlight it stands
        // in for.
        let file = file(
            "src/lib.rs",
            &[
                (LineKind::Context, "let x = 1;", 1),
                (LineKind::Added, "// note", 2),
            ],
        );

        let mut highlighter = Highlighter::with_theme(TEST_THEME).unwrap();
        let parsed = highlighter.parse_side(&file, Side::After);

        // Under the parse theme, coloring the cache matches a direct highlight.
        k9::assert_equal!(
            highlighter.color_side(&parsed),
            highlighter.highlight_side(&file, Side::After)
        );

        // After switching, the same cached parse recolors to the new theme
        // without re-parsing, matching a highlighter built fresh on that theme.
        const OTHER_THEME: &str = "InspiredGitHub";
        highlighter.set_theme(OTHER_THEME).unwrap();
        let fresh = Highlighter::with_theme(OTHER_THEME).unwrap();
        k9::assert_equal!(
            highlighter.color_side(&parsed),
            fresh.highlight_side(&file, Side::After)
        );

        // An unknown theme is rejected and leaves the coloring untouched.
        k9::assert_equal!(
            highlighter.set_theme("no-such-theme").err(),
            Some(HighlightError::UnknownTheme {
                name: "no-such-theme".to_string(),
            })
        );
        k9::assert_equal!(
            highlighter.color_side(&parsed),
            fresh.highlight_side(&file, Side::After)
        );
    }
}
