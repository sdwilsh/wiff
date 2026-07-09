//! Syntax highlighting of file content with syntect.
//!
//! Highlighting a diff means highlighting the file content it shows, so a
//! [`Highlighter`] reconstructs one side of a file and colors its known lines,
//! returning each line's colored spans keyed by line number for the renderer to
//! place against the diff. The spans are backend-neutral ([`Rgb`] plus bold and
//! friends) rather than syntect or ratatui types, so nothing downstream takes a
//! syntect dependency and the output is straightforward to assert.

use std::collections::BTreeMap;

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::line::LineNo;
use crate::model::{FileDiff, Side};
use crate::reconstitute::{ReconLine, reconstitute};

/// The default theme for a dark terminal.
pub const DEFAULT_DARK_THEME: &str = "base16-ocean.dark";

/// The default theme for a light terminal.
pub const DEFAULT_LIGHT_THEME: &str = "InspiredGitHub";

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

    /// Highlight the known lines of `side` of `file`, keyed by line number.
    ///
    /// The file's path selects the syntax; gaps in the reconstruction break the
    /// highlighter's state, since the omitted lines could carry multi-line
    /// constructs whose effect cannot be known.
    pub fn highlight_side(&self, file: &FileDiff, side: Side) -> BTreeMap<LineNo, HighlightedLine> {
        let syntax = self.syntax_for(file.display_path());
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut out = BTreeMap::new();
        for line in reconstitute(file, side) {
            match line {
                ReconLine::Known { lineno, text } => {
                    out.insert(lineno, self.highlight_line(&mut highlighter, &text));
                }
                // Restart highlighting past a gap: the highlighter's state
                // reflects only the lines it has seen, so a gap resets it.
                ReconLine::Gap { .. } => {
                    highlighter = HighlightLines::new(syntax, &self.theme);
                }
            }
        }
        out
    }

    /// Highlight one line of text into colored spans.
    fn highlight_line(&self, highlighter: &mut HighlightLines, text: &str) -> HighlightedLine {
        // syntect's newline-aware syntaxes expect a trailing newline to close
        // line-scoped constructs; it never appears in the returned spans.
        let with_newline = format!("{text}\n");
        let ranges = highlighter
            .highlight_line(&with_newline, &self.syntaxes)
            .unwrap_or_default();
        ranges
            .into_iter()
            .map(|(style, piece)| StyledSpan {
                text: piece.trim_end_matches('\n').to_string(),
                style: convert_style(style),
            })
            .filter(|span| !span.text.is_empty())
            .collect()
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
}
