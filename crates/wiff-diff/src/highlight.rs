//! Syntax highlighting of file content with syntect.
//!
//! Highlighting a diff means highlighting the file content it shows, so a
//! [`Highlighter`] reconstructs one side of a file and colors its known lines,
//! returning each line's colored spans keyed by line number for the renderer to
//! place against the diff. The spans are backend-neutral ([`Rgb`] plus bold and
//! friends) rather than syntect or ratatui types, so nothing downstream takes a
//! syntect dependency and the output is straightforward to assert.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter as ThemeHighlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};
use two_face::theme::LazyThemeSet;

use crate::line::LineNo;
use crate::model::{FileDiff, Side};
use crate::reconstitute::{ReconLine, reconstitute};

/// The default theme for a dark terminal.
pub const DEFAULT_DARK_THEME: &str = "wez";

/// The default theme for a light terminal.
pub const DEFAULT_LIGHT_THEME: &str = "InspiredGitHub";

/// The bundled "wez" theme, a dark palette translated from the author's vim
/// colorscheme. It is the default dark theme, named by [`DEFAULT_DARK_THEME`].
const WEZ_THEME: &str = include_str!("../assets/wez.tmTheme");

/// The newline-aware syntaxes, bat's expanded grammar collection by way of
/// two-face, built once and shared. It reaches languages syntect's own defaults
/// omit, such as TOML and TypeScript.
static SYNTAXES: LazyLock<Arc<SyntaxSet>> =
    LazyLock::new(|| Arc::new(two_face::syntax::extra_newlines()));

/// The selectable syntax themes, bat's curated collection by way of two-face.
/// Each theme is decompressed on first use, so listing the names or reading one
/// theme does not pay for the whole collection. The bundled wez theme is kept
/// separately in [`WEZ`] and folded in by [`theme`].
static THEMES: LazyLock<LazyThemeSet> =
    LazyLock::new(|| LazyThemeSet::from(two_face::theme::extra()));

/// The bundled wez theme, parsed once from [`WEZ_THEME`].
static WEZ: LazyLock<Theme> = LazyLock::new(|| {
    ThemeSet::load_from_reader(&mut std::io::Cursor::new(WEZ_THEME))
        .expect("bundled wez theme parses")
});

/// The built-in theme `name`, the bundled wez palette or one of bat's themes,
/// or `None` when no such theme is bundled or it is not a true-color theme.
fn theme(name: &str) -> Option<Theme> {
    if name == DEFAULT_DARK_THEME {
        return Some(WEZ.clone());
    }
    THEMES
        .get(name)
        .filter(|theme| is_truecolor(theme))
        .cloned()
}

/// Whether `theme` is built for true color rather than a terminal's 16-color
/// ANSI palette. two-face bundles a few ANSI themes whose settings hold palette
/// indices in place of RGB, marked by a non-opaque background; wiff derives its
/// whole interface from RGB and cannot render those, so it leaves them out.
fn is_truecolor(theme: &Theme) -> bool {
    theme.settings.background.is_none_or(|c| c.a == 255)
}

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
    /// The background washed over the line the cursor is on.
    pub line_highlight: Option<Rgb>,
    /// The background of the active selection.
    pub selection: Option<Rgb>,
    /// The text color over the active selection.
    pub selection_foreground: Option<Rgb>,
    /// The background of a search match.
    pub find_highlight: Option<Rgb>,
}

/// The names of the built-in syntax themes, sorted, for listing the choices a
/// reviewer can switch between.
pub fn theme_names() -> Vec<String> {
    let mut names: Vec<String> = THEMES
        .theme_names()
        .filter(|name| THEMES.get(name).is_some_and(is_truecolor))
        .map(String::from)
        .collect();
    names.push(DEFAULT_DARK_THEME.to_string());
    names.sort();
    names
}

/// The chrome colors of the built-in theme `name`, or `None` when no such theme
/// is bundled.
pub fn theme_chrome(name: &str) -> Option<ThemeChrome> {
    let theme = theme(name)?;
    let settings = &theme.settings;
    Some(ThemeChrome {
        background: settings.background.map(rgb_of),
        foreground: settings.foreground.map(rgb_of),
        gutter_foreground: settings.gutter_foreground.map(rgb_of),
        line_highlight: settings.line_highlight.map(rgb_of),
        selection: settings.selection.map(rgb_of),
        selection_foreground: settings.selection_foreground.map(rgb_of),
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
/// and markdown renderers agree. The syntaxes name some languages in ways no
/// markdown fence understands ("Bourne Again Shell (bash)"), so the fence token
/// is taken from this table rather than from the matched syntax.
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
        let theme = theme(name).ok_or_else(|| HighlightError::UnknownTheme {
            name: name.to_string(),
        })?;
        Ok(Self {
            syntaxes: SYNTAXES.clone(),
            theme,
        })
    }

    /// Recolor to the named built-in theme, keeping the loaded syntaxes so a
    /// caller can recolor a cached parse instead of highlighting the diff
    /// afresh. On an unknown theme the highlighter is left unchanged.
    pub fn set_theme(&mut self, name: &str) -> Result<(), HighlightError> {
        self.theme = theme(name).ok_or_else(|| HighlightError::UnknownTheme {
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

    /// A colored span with font flags, for fixtures that assert emphasis.
    fn styled(text: &str, hex: &str, bold: bool, italic: bool, underline: bool) -> StyledSpan {
        StyledSpan {
            text: text.to_string(),
            style: Style {
                fg: rgb(hex),
                bold,
                italic,
                underline,
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
        wince::assert_eq!(highlighter.highlight_side(&file, Side::After), expected);
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
        wince::assert_eq!(
            highlighter.highlight_code("rust", "let x = 1;\n// note"),
            Some(expected)
        );
    }

    #[test]
    fn highlight_code_returns_none_for_an_unknown_language() {
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();
        wince::assert_eq!(highlighter.highlight_code("nonesuch", "fn main() {}"), None);
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
        wince::assert_eq!(highlighter.highlight_side(&file, Side::Before), expected);
    }

    #[test]
    fn an_unknown_extension_falls_back_to_one_plain_span() {
        let file = file("notes.unknownext", &[(LineKind::Context, "hello world", 1)]);
        let highlighter = Highlighter::with_theme(TEST_THEME).unwrap();

        let expected: BTreeMap<LineNo, HighlightedLine> =
            [(ln(1), vec![span("hello world", "#c0c5ce")])]
                .into_iter()
                .collect();
        wince::assert_eq!(highlighter.highlight_side(&file, Side::After), expected);
    }

    #[test]
    fn fence_language_maps_known_extensions_and_ignores_the_rest() {
        use super::fence_language;
        wince::assert_eq!(fence_language("src/lib.rs"), Some("rust"));
        wince::assert_eq!(fence_language("deploy.sh"), Some("bash"));
        wince::assert_eq!(fence_language("conf.yml"), Some("yaml"));
        wince::assert_eq!(fence_language("Cargo.toml"), Some("toml"));
        wince::assert_eq!(fence_language("notes.unknownext"), None);
        wince::assert_eq!(fence_language("Makefile"), None);
    }

    #[test]
    fn an_unknown_theme_is_an_error() {
        wince::assert_eq!(
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
        wince::assert_eq!(
            highlighter.color_side(&parsed),
            highlighter.highlight_side(&file, Side::After)
        );

        // After switching, the same cached parse recolors to the new theme
        // without re-parsing, matching a highlighter built fresh on that theme.
        const OTHER_THEME: &str = "InspiredGitHub";
        highlighter.set_theme(OTHER_THEME).unwrap();
        let fresh = Highlighter::with_theme(OTHER_THEME).unwrap();
        wince::assert_eq!(
            highlighter.color_side(&parsed),
            fresh.highlight_side(&file, Side::After)
        );

        // An unknown theme is rejected and leaves the coloring untouched.
        wince::assert_eq!(
            highlighter.set_theme("no-such-theme").err(),
            Some(HighlightError::UnknownTheme {
                name: "no-such-theme".to_string(),
            })
        );
        wince::assert_eq!(
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
        wince::assert_eq!(
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
    fn the_wez_theme_colors_markdown_markup() {
        // The bundled wez theme names the markdown markup scopes with the same
        // colors the rendered-comment view draws, so composing a comment shows
        // headings, emphasis, code, and links in color rather than flat
        // foreground and previews how the saved comment will look.
        let highlighter = Highlighter::with_theme("wez").unwrap();
        let mut live = highlighter.live("markdown");
        live.update(&[
            "# Heading".to_string(),
            "**bold** *italic* `code`".to_string(),
            "[link](http://x.com)".to_string(),
            "- item".to_string(),
        ]);
        wince::assert_eq!(
            live_spans(&live, 4),
            vec![
                vec![
                    styled("#", "#ebcb8b", true, false, false),
                    styled(" ", "#ebcb8b", true, false, false),
                    styled("Heading", "#ebcb8b", true, false, false),
                ],
                vec![
                    styled("**", "#c3c3c3", true, false, false),
                    styled("bold", "#c3c3c3", true, false, false),
                    styled("**", "#c3c3c3", true, false, false),
                    span(" ", "#c3c3c3"),
                    styled("*", "#c3c3c3", false, true, false),
                    styled("italic", "#c3c3c3", false, true, false),
                    styled("*", "#c3c3c3", false, true, false),
                    span(" ", "#c3c3c3"),
                    span("`", "#96b5b4"),
                    span("code", "#96b5b4"),
                    span("`", "#96b5b4"),
                ],
                vec![
                    span("[", "#c3c3c3"),
                    span("link", "#c3c3c3"),
                    span("]", "#c3c3c3"),
                    span("(", "#c3c3c3"),
                    styled("http://x.com", "#8fa1b3", false, false, true),
                    span(")", "#c3c3c3"),
                ],
                vec![
                    span("-", "#c3c3c3"),
                    span(" ", "#c3c3c3"),
                    span("item", "#c3c3c3"),
                ],
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

        wince::assert_eq!(live_spans(&typed, 3), live_spans(&fresh, 3));
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
        wince::assert_eq!(live_spans(&typed, 1), live_spans(&fresh, 1));

        // Grow again and the appended line colors as a fresh two-line highlight.
        typed.update(&["# Note".to_string(), "tail".to_string()]);
        let mut grown = highlighter.live("markdown");
        grown.update(&["# Note".to_string(), "tail".to_string()]);
        wince::assert_eq!(live_spans(&typed, 2), live_spans(&grown, 2));
    }
}
