//! Recognising the enclosing-definition line for a run of lines: the function,
//! struct, heading, and so on that a change sits inside, shown on a fold marker.
//!
//! A wide-context capture merges each file's changes into one hunk, so there is
//! no per-change context header to name that scope. We recover it at render time
//! by scanning the file content upward for the nearest definition line, with two
//! strategies:
//!
//! - With no language rules, a default heuristic ([`is_default_definition`]):
//!   a line whose first byte begins an identifier. Body lines are indented, so
//!   the nearest such line above a change is its enclosing top-level item.
//! - With a language driver, an ordered list of regexes evaluated first-match-
//!   wins: the first pattern to match decides, an exclusion (written with a
//!   leading `!`) rejecting the line. The `!`-exclusion syntax matches git's
//!   `userdiff` config format, so a user can paste git's patterns into config.

use std::collections::BTreeMap;

use fancy_regex::Regex;

use crate::highlight::fence_language;

/// The definition-line patterns for each language wiff ships, keyed by the
/// language token [`fence_language`] yields. Each recognises where a top-level
/// item or heading starts; a leading `!` on a pattern marks it an exclusion.
const BUILTIN: &[(&str, &[&str])] = &[
    (
        "rust",
        &[
            r#"^[\t ]*(pub([\t ]*\([^)]*\))?[\t ]+)?(default[\t ]+)?(async[\t ]+)?(unsafe[\t ]+)?(extern([\t ]+"[^"]*")?[\t ]+)?(fn|struct|enum|union|trait|impl|mod|type|const|static|macro_rules!)\b"#,
        ],
    ),
    ("python", &[r"^[\t ]*(class|def|async[\t ]+def)[\t ]+"]),
    ("markdown", &[r"^ {0,3}#{1,6}[\t ]"]),
];

/// An error compiling a section pattern into a matcher.
#[derive(Debug, thiserror::Error)]
pub enum SectionError {
    /// A language's pattern was not a valid regex.
    #[error("invalid section pattern for {language}: {source}")]
    Pattern {
        /// The language the pattern belongs to.
        language: String,
        /// The underlying regex compilation error.
        source: Box<fancy_regex::Error>,
    },
}

/// One compiled pattern and whether matching it rejects the line rather than
/// accepting it as a definition.
struct Rule {
    regex: Regex,
    negate: bool,
}

/// An ordered list of rules recognising one language's definition lines,
/// evaluated first-match-wins.
struct LanguageRules {
    rules: Vec<Rule>,
}

impl LanguageRules {
    /// Compile `patterns` for `language`, treating a leading `!` as an exclusion.
    fn compile<S: AsRef<str>>(language: &str, patterns: &[S]) -> Result<Self, SectionError> {
        let mut rules = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            let (negate, body) = match pattern.as_ref().strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, pattern.as_ref()),
            };
            let regex = Regex::new(body).map_err(|source| SectionError::Pattern {
                language: language.to_string(),
                source: Box::new(source),
            })?;
            rules.push(Rule { regex, negate });
        }
        Ok(Self { rules })
    }

    /// Whether `line` is a definition: the first rule to match decides, an
    /// exclusion rejecting the line, and no match rejecting it too.
    fn is_definition(&self, line: &str) -> bool {
        for rule in &self.rules {
            if rule.regex.is_match(line).unwrap_or(false) {
                return !rule.negate;
            }
        }
        false
    }
}

/// The definition-line rules for one path: a language driver, or the default
/// heuristic when the language has none.
pub struct Section<'a> {
    rules: Option<&'a LanguageRules>,
}

impl Section<'_> {
    /// Whether `line` begins an enclosing definition.
    pub fn is_definition(&self, line: &str) -> bool {
        match self.rules {
            Some(rules) => rules.is_definition(line),
            None => is_default_definition(line),
        }
    }
}

/// The compiled section matchers for every language, the built-ins overlaid with
/// the user's configured patterns.
pub struct SectionMatchers {
    by_language: BTreeMap<String, LanguageRules>,
}

impl SectionMatchers {
    /// Build the matchers from the built-in patterns, with each language in
    /// `overrides` replacing its built-in patterns (and new languages added).
    pub fn new(overrides: &BTreeMap<String, Vec<String>>) -> Result<Self, SectionError> {
        let mut by_language = BTreeMap::new();
        for (language, patterns) in BUILTIN {
            by_language.insert(
                language.to_string(),
                LanguageRules::compile(language, patterns)?,
            );
        }
        for (language, patterns) in overrides {
            by_language.insert(
                language.clone(),
                LanguageRules::compile(language, patterns)?,
            );
        }
        Ok(Self { by_language })
    }

    /// The matchers with only the built-in patterns.
    pub fn builtins() -> Self {
        Self::new(&BTreeMap::new()).expect("built-in section patterns are valid regexes")
    }

    /// The section rules for `path`, resolved through its language.
    pub fn for_path(&self, path: &str) -> Section<'_> {
        let rules = fence_language(path).and_then(|language| self.by_language.get(language));
        Section { rules }
    }
}

/// The default definition heuristic: a line whose first byte begins an
/// identifier, on the convention that top-level items sit at column zero while
/// their bodies are indented.
fn is_default_definition(line: &str) -> bool {
    match line.bytes().next() {
        Some(c) => c.is_ascii_alphabetic() || c == b'_' || c == b'$',
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::SectionMatchers;

    /// Every definition verdict for `lines` under the matcher for `path`, one
    /// `text -> bool` row per line so the whole classification is asserted.
    fn classify(matchers: &SectionMatchers, path: &str, lines: &[&str]) -> String {
        let section = matchers.for_path(path);
        let mut out = String::new();
        for line in lines {
            out.push_str(&format!("{} -> {}\n", line, section.is_definition(line)));
        }
        out
    }

    #[test]
    fn the_default_heuristic_marks_column_zero_identifier_lines() {
        // An unknown language falls back to the default heuristic: a leading
        // identifier char is a definition, indentation or punctuation is not,
        // and a blank line is not.
        let matchers = SectionMatchers::builtins();
        let expected = "\
struct Thing { -> true
_private() -> true
$shell -> true
    indented(); -> false
} -> false
 -> false
";
        k9::assert_equal!(
            classify(
                &matchers,
                "notes.txt",
                &[
                    "struct Thing {",
                    "_private()",
                    "$shell",
                    "    indented();",
                    "}",
                    "",
                ]
            ),
            expected.to_string()
        );
    }

    #[test]
    fn the_rust_driver_matches_indented_definitions_but_not_statements() {
        // The ported rust pattern allows leading whitespace, so a method inside
        // an impl is a definition, while ordinary statements are not.
        let matchers = SectionMatchers::builtins();
        let expected = "\
pub fn draw() { -> true
impl Theme { -> true
    fn method(&self) -> u8 { -> true
    let x = 1; -> false
} -> false
";
        k9::assert_equal!(
            classify(
                &matchers,
                "src/lib.rs",
                &[
                    "pub fn draw() {",
                    "impl Theme {",
                    "    fn method(&self) -> u8 {",
                    "    let x = 1;",
                    "}",
                ]
            ),
            expected.to_string()
        );
    }

    #[test]
    fn the_python_and_markdown_drivers_match_their_definitions() {
        let matchers = SectionMatchers::builtins();
        let python = "\
class Widget: -> true
    async def load(self): -> true
    x = 1 -> false
";
        k9::assert_equal!(
            classify(
                &matchers,
                "app.py",
                &["class Widget:", "    async def load(self):", "    x = 1",]
            ),
            python.to_string()
        );
        let markdown = "\
## Heading -> true
###### Deep -> true
Just a paragraph. -> false
";
        k9::assert_equal!(
            classify(
                &matchers,
                "README.md",
                &["## Heading", "###### Deep", "Just a paragraph.",]
            ),
            markdown.to_string()
        );
    }

    #[test]
    fn a_configured_language_replaces_its_builtin_and_honors_exclusions() {
        // Replacing rust with a rule set whose first rule excludes attribute
        // lines: the exclusion wins over the following positive even though both
        // would match.
        let overrides: BTreeMap<String, Vec<String>> = [(
            "rust".to_string(),
            vec![
                r"!^[\t ]*#\[".to_string(),
                r"^[\t ]*(pub[\t ]+)?fn[\t ].*$".to_string(),
            ],
        )]
        .into_iter()
        .collect();
        let matchers = SectionMatchers::new(&overrides).unwrap();
        let expected = "\
pub fn draw() { -> true
#[derive(Debug)] -> false
struct Theme { -> false
";
        // `struct` is no longer a definition since the replacement only knows
        // about `fn`, and the attribute line is excluded.
        k9::assert_equal!(
            classify(
                &matchers,
                "src/lib.rs",
                &["pub fn draw() {", "#[derive(Debug)]", "struct Theme {",]
            ),
            expected.to_string()
        );
    }

    #[test]
    fn an_invalid_configured_pattern_is_an_error() {
        let overrides: BTreeMap<String, Vec<String>> =
            [("go".to_string(), vec!["(unclosed".to_string()])]
                .into_iter()
                .collect();
        let message = match SectionMatchers::new(&overrides) {
            Ok(_) => panic!("expected an invalid-pattern error"),
            Err(error) => error.to_string(),
        };
        k9::assert_equal!(
            message,
            "invalid section pattern for go: Parsing error at position 9: Opening parenthesis without closing parenthesis".to_string()
        );
    }
}
