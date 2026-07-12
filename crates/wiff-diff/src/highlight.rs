//! Syntax highlighting of file content with syntect.
//!
//! Highlighting a diff means highlighting the file content it shows, so a
//! [`Highlighter`] reconstructs one side of a file and colors its known lines,
//! returning each line's colored spans keyed by line number for the renderer to
//! place against the diff. The spans are backend-neutral ([`Rgb`] plus bold and
//! friends) rather than syntect or ratatui types, so nothing downstream takes a
//! syntect dependency and the output is straightforward to assert.

use std::collections::BTreeMap;
use std::sync::Arc;

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
    syntaxes: Arc<SyntaxSet>,
    theme: Theme,
}

/// A cheaply cloneable, thread-safe view of just the syntaxes, so a file can be
/// parsed off the main thread without borrowing the whole [`Highlighter`]. It
/// holds no theme: parsing is theme-independent, and coloring the result is
/// left to whichever theme is current when it runs.
#[derive(Clone)]
pub struct Parser {
    syntaxes: Arc<SyntaxSet>,
}

impl Parser {
    /// Parse the known lines of `side` of `file` into their scope operations,
    /// the same theme-independent work [`Highlighter::parse_side`] does, but
    /// usable from a worker thread.
    pub fn parse_side(&self, file: &FileDiff, side: Side) -> ParsedSide {
        parse_side_with(&self.syntaxes, file, side)
    }
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
            syntaxes: Arc::new(SyntaxSet::load_defaults_newlines()),
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
    /// The file's path selects the syntax.
    pub fn highlight_side(&self, file: &FileDiff, side: Side) -> BTreeMap<LineNo, HighlightedLine> {
        self.color_side(&self.parse_side(file, side))
    }

    /// Parse the known lines of `side` of `file` into their scope operations:
    /// the costly, theme-independent half of highlighting. The file's path
    /// selects the syntax.
    pub fn parse_side(&self, file: &FileDiff, side: Side) -> ParsedSide {
        parse_side_with(&self.syntaxes, file, side)
    }

    /// Return a [`Parser`] that shares this highlighter's syntaxes, for parsing
    /// files off the main thread while the highlighter stays put to color the
    /// results.
    pub fn parser(&self) -> Parser {
        Parser {
            syntaxes: Arc::clone(&self.syntaxes),
        }
    }

    /// An incremental highlighter for `token`'s syntax, sharing this
    /// highlighter's syntaxes and current theme.
    pub fn live(&self, token: &str) -> LiveHighlighter {
        LiveHighlighter::new(Arc::clone(&self.syntaxes), self.theme.clone(), token)
    }

    /// Highlight a standalone block of `code` as `token`'s language, returning
    /// one entry of colored spans per source line. Returns `None` when `token`
    /// names no known syntax, so a caller can fall back to plain rendering.
    pub fn highlight_code(&self, token: &str, code: &str) -> Option<Vec<HighlightedLine>> {
        let syntax = self.syntaxes.find_syntax_by_token(token)?;
        let highlighter = ThemeHighlighter::new(&self.theme);
        let mut parse = ParseState::new(syntax);
        let mut state = HighlightState::new(&highlighter, ScopeStack::new());
        let mut out = Vec::new();
        // A trailing newline would otherwise split into a spurious empty final
        // line; the block's lines are the text between the newlines.
        for line in code.strip_suffix('\n').unwrap_or(code).split('\n') {
            // syntect's newline-aware syntaxes expect a trailing newline to
            // close line-scoped constructs; coloring trims it back off.
            let text = format!("{line}\n");
            let ops = parse.parse_line(&text, &self.syntaxes).unwrap_or_default();
            let spans = HighlightIterator::new(&mut state, &ops, &text, &highlighter)
                .map(|(style, piece)| StyledSpan {
                    text: piece.trim_end_matches('\n').to_string(),
                    style: convert_style(style),
                })
                .filter(|span| !span.text.is_empty())
                .collect();
            out.push(spans);
        }
        Some(out)
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
}

/// Parse the known lines of `side` of `file` into their scope operations against
/// `syntaxes`. The file's path selects the syntax.
fn parse_side_with(syntaxes: &SyntaxSet, file: &FileDiff, side: Side) -> ParsedSide {
    let syntax = syntax_for(syntaxes, file.display_path());
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
                let ops = state.parse_line(&text, syntaxes).unwrap_or_default();
                run.push(ParsedLine { lineno, text, ops });
            }
            ReconLine::Gap { .. } => {
                // A gap hides an unknown span of the file; a multi-line
                // construct could open inside it, so end the current run and
                // restart the parse state rather than color the next lines on
                // state that assumes contiguous input.
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

/// The syntax for a file path, falling back to plain text when none matches.
fn syntax_for<'a>(syntaxes: &'a SyntaxSet, path: &str) -> &'a SyntaxReference {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(|ext| syntaxes.find_syntax_by_extension(ext))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
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

/// The parse and highlight state at a line boundary, cached so an edit can
/// resume coloring from an unchanged line instead of from the top of the
/// buffer.
#[derive(Clone, PartialEq)]
struct LineState {
    parse: ParseState,
    highlight: HighlightState,
}

#[derive(Clone)]
struct LiveLine {
    text: String,
    spans: HighlightedLine,
}

/// An incremental highlighter for a small, live-edited buffer such as a comment
/// editor. Reparsing the whole buffer on every keystroke would dominate the
/// cost of typing; caching each line's coloring keeps an edit's work
/// proportional to what changed rather than the whole buffer.
pub struct LiveHighlighter {
    syntaxes: Arc<SyntaxSet>,
    theme: Theme,
    /// The line boundaries: `boundaries[i]` is the state entering line `i`, with
    /// a terminal entry after the last line, so it is one longer than `lines`.
    boundaries: Vec<LineState>,
    lines: Vec<LiveLine>,
}

impl LiveHighlighter {
    /// Build a highlighter for `token`'s syntax over `syntaxes` and `theme`,
    /// with an empty buffer. An unknown token falls back to plain text.
    fn new(syntaxes: Arc<SyntaxSet>, theme: Theme, token: &str) -> Self {
        let syntax = syntaxes
            .find_syntax_by_token(token)
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
        let theme_highlighter = ThemeHighlighter::new(&theme);
        let initial = LineState {
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&theme_highlighter, ScopeStack::new()),
        };
        Self {
            syntaxes,
            theme,
            boundaries: vec![initial],
            lines: Vec::new(),
        }
    }

    /// Recolor the buffer to `lines`.
    pub fn update(&mut self, lines: &[String]) {
        let theme_highlighter = ThemeHighlighter::new(&self.theme);
        let mut first = 0;
        while first < self.lines.len()
            && first < lines.len()
            && self.lines[first].text == lines[first]
        {
            first += 1;
        }
        // A cursor-only move leaves every line unchanged; nothing to recolor.
        if first == self.lines.len() && first == lines.len() {
            return;
        }
        let mut state = self.boundaries[first].clone();
        // Reused across lines to hold the current line plus the trailing newline
        // syntect expects, avoiding a fresh allocation per line per keystroke.
        let mut text = String::new();
        let mut new_lines = self.lines[..first].to_vec();
        let mut new_boundaries = self.boundaries[..=first].to_vec();
        let mut i = first;
        while i < lines.len() {
            // Past the first change, an unchanged line reached with the same
            // entering state reproduces the cached tail, boundaries and all.
            // This convergence check leans on syntect's `PartialEq` for the
            // parse and highlight state reflecting only what affects coloring;
            // were it to compare transient internal fields, convergence would be
            // missed and the tail recolored needlessly, still correct but
            // slower.
            if i < self.lines.len() && lines[i] == self.lines[i].text && state == self.boundaries[i]
            {
                new_lines.extend_from_slice(&self.lines[i..]);
                new_boundaries.extend_from_slice(&self.boundaries[i + 1..]);
                self.lines = new_lines;
                self.boundaries = new_boundaries;
                return;
            }
            // syntect's newline-aware syntaxes expect a trailing newline to
            // close line-scoped constructs; coloring trims it back off.
            text.clear();
            text.push_str(&lines[i]);
            text.push('\n');
            let ops = state
                .parse
                .parse_line(&text, &self.syntaxes)
                .unwrap_or_default();
            let spans =
                HighlightIterator::new(&mut state.highlight, &ops, &text, &theme_highlighter)
                    .map(|(style, piece)| StyledSpan {
                        text: piece.trim_end_matches('\n').to_string(),
                        style: convert_style(style),
                    })
                    .filter(|span| !span.text.is_empty())
                    .collect();
            new_lines.push(LiveLine {
                text: lines[i].clone(),
                spans,
            });
            new_boundaries.push(state.clone());
            i += 1;
        }
        self.lines = new_lines;
        self.boundaries = new_boundaries;
    }

    /// Returns the colored spans of line `index`, or an empty slice when out of
    /// range.
    pub fn line_spans(&self, index: usize) -> &[StyledSpan] {
        self.lines
            .get(index)
            .map_or(&[], |line| line.spans.as_slice())
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
    fn highlight_code_colors_a_standalone_block_by_language() {
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();
        let expected = vec![
            vec![
                span("let", "#b48ead"),
                span(" x ", "#c0c5ce"),
                span("=", "#c0c5ce"),
                span(" ", "#c0c5ce"),
                span("1", "#d08770"),
                span(";", "#c0c5ce"),
            ],
            vec![span("//", "#65737e"), span(" note", "#65737e")],
        ];
        k9::assert_equal!(
            highlighter.highlight_code("rust", "let x = 1;\n// note"),
            Some(expected)
        );
    }

    #[test]
    fn highlight_code_returns_none_for_an_unknown_language() {
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();
        k9::assert_equal!(highlighter.highlight_code("nonesuch", "fn main() {}"), None);
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

    /// Every line's colored spans, for asserting a live buffer in full.
    fn live_spans(live: &super::LiveHighlighter, count: usize) -> Vec<HighlightedLine> {
        (0..count).map(|i| live.line_spans(i).to_vec()).collect()
    }

    #[test]
    fn a_live_highlighter_colors_markdown_source() {
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();
        let mut live = highlighter.live("markdown");
        live.update(&["# Title".to_string(), "plain text".to_string()]);

        // A heading takes the theme's heading color while ordinary prose stays
        // the default foreground, proving the markdown syntax resolved.
        k9::assert_equal!(
            live_spans(&live, 2),
            vec![
                vec![
                    span("#", "#8fa1b3"),
                    span(" ", "#c0c5ce"),
                    span("Title", "#8fa1b3"),
                ],
                vec![span("plain text", "#c0c5ce")],
            ]
        );
    }

    #[test]
    fn a_live_edit_matches_a_fresh_highlight_of_the_final_buffer() {
        // Correctness of the incremental path: typing a buffer up line by line,
        // then editing an early line, must leave every line colored exactly as
        // highlighting the final buffer in one pass would. A fenced code block
        // spans lines, so an early edit must recolor the lines below it.
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();
        let mut typed = highlighter.live("markdown");
        typed.update(&["# Note".to_string()]);
        typed.update(&["# Note".to_string(), "```rust".to_string()]);
        typed.update(&[
            "# Note".to_string(),
            "```rust".to_string(),
            "let x = 1;".to_string(),
        ]);
        // Edit the first line: the fenced block below it must stay colored.
        typed.update(&[
            "# Heading".to_string(),
            "```rust".to_string(),
            "let x = 1;".to_string(),
        ]);

        let final_buffer = [
            "# Heading".to_string(),
            "```rust".to_string(),
            "let x = 1;".to_string(),
        ];
        let mut fresh = highlighter.live("markdown");
        fresh.update(&final_buffer);

        k9::assert_equal!(live_spans(&typed, 3), live_spans(&fresh, 3));
    }

    #[test]
    fn a_live_shrink_matches_a_fresh_highlight_of_the_shorter_buffer() {
        // Deleting trailing lines drives the tail-trimming branch and must keep
        // the boundaries one longer than the lines. After typing three lines and
        // shrinking back to one, the remaining line stays colored as a fresh
        // one-pass highlight of the one-line buffer, and coloring can still
        // continue correctly when the buffer grows again.
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();
        let mut typed = highlighter.live("markdown");
        typed.update(&[
            "# Note".to_string(),
            "first".to_string(),
            "second".to_string(),
        ]);
        typed.update(&["# Note".to_string()]);

        let mut fresh = highlighter.live("markdown");
        fresh.update(&["# Note".to_string()]);
        k9::assert_equal!(live_spans(&typed, 1), live_spans(&fresh, 1));

        // Grow again and the appended line colors as a fresh two-line highlight.
        typed.update(&["# Note".to_string(), "tail".to_string()]);
        let mut grown = highlighter.live("markdown");
        grown.update(&["# Note".to_string(), "tail".to_string()]);
        k9::assert_equal!(live_spans(&typed, 2), live_spans(&grown, 2));
    }
}
