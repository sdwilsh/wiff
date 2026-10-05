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

use std::collections::{BTreeMap, BTreeSet};

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

/// The prefixes, tested after leading whitespace, that mark a line as part of
/// the comment or attribute block leading into a definition, keyed by language.
/// A blank line attaches regardless and is not listed here.
const BUILTIN_ATTACHMENT: &[(&str, &[&str])] = &[
    ("rust", &["//", "/*", "*", "#[", "#!"]),
    ("python", &["#", "@"]),
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

/// Recognizes definitions and their leading comment or attribute lines for one
/// language.
struct LanguageRules {
    rules: Vec<Rule>,
    attachment: Vec<String>,
}

impl LanguageRules {
    /// Compile `patterns` for `language`, treating a leading `!` as an
    /// exclusion. `attachment` holds the comment/attribute prefixes for the
    /// language.
    fn compile<S: AsRef<str>>(
        language: &str,
        patterns: &[S],
        attachment: Vec<String>,
    ) -> Result<Self, SectionError> {
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
        Ok(Self { rules, attachment })
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

    /// Whether `line`, after its leading whitespace, opens with a comment or
    /// attribute prefix for this language.
    fn is_attachment(&self, line: &str) -> bool {
        let trimmed = line.trim_start();
        self.attachment
            .iter()
            .any(|prefix| trimmed.starts_with(prefix.as_str()))
    }
}

/// Returns the attachment prefixes `language` includes.
fn builtin_attachment(language: &str) -> Vec<String> {
    BUILTIN_ATTACHMENT
        .iter()
        .find(|(name, _)| *name == language)
        .map(|(_, prefixes)| prefixes.iter().map(|p| p.to_string()).collect())
        .unwrap_or_default()
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

    /// Returns whether `line` may belong to a leading comment or attribute
    /// block. Blank lines qualify for every file. Recognized comment and
    /// attribute prefixes qualify only for languages with configured rules.
    pub fn is_attachment(&self, line: &str) -> bool {
        if line.trim().is_empty() {
            return true;
        }
        match self.rules {
            Some(rules) => rules.is_attachment(line),
            None => false,
        }
    }
}

/// The compiled section matchers for every language, the built-ins overlaid with
/// the user's configured patterns.
pub struct SectionMatchers {
    by_language: BTreeMap<String, LanguageRules>,
}

impl SectionMatchers {
    /// Build the matchers from the built-ins layered with the user's `sections`
    /// (definition patterns) and `attachment` (comment/attribute prefixes)
    /// overrides. `sections` and `attachment` are independent: overriding one for
    /// a language does not clear the other, which stays at its built-in value for
    /// that language.
    pub fn new(
        sections: &BTreeMap<String, Vec<String>>,
        attachment: &BTreeMap<String, Vec<String>>,
    ) -> Result<Self, SectionError> {
        // Assemble the definition patterns of each language first: the
        // built-ins, then each listed language replacing its definition
        // patterns wholesale.
        let mut definitions: BTreeMap<String, Vec<String>> = BUILTIN
            .iter()
            .map(|(language, patterns)| {
                (
                    (*language).to_string(),
                    patterns.iter().map(|p| (*p).to_string()).collect(),
                )
            })
            .collect();
        for (language, patterns) in sections {
            definitions.insert(language.clone(), patterns.clone());
        }
        // A language the user configured only for attachment, without any
        // definition patterns of its own, still needs an entry: we want it
        // available for attachment matching even though it will never match a
        // definition.
        let languages: BTreeSet<&str> = definitions
            .keys()
            .chain(attachment.keys())
            .map(String::as_str)
            .collect();
        let mut by_language = BTreeMap::new();
        for language in languages {
            let patterns = definitions.get(language).cloned().unwrap_or_default();
            let prefixes = attachment
                .get(language)
                .cloned()
                .unwrap_or_else(|| builtin_attachment(language));
            by_language.insert(
                language.to_string(),
                LanguageRules::compile(language, &patterns, prefixes)?,
            );
        }
        Ok(Self { by_language })
    }

    /// The matchers with only the built-in patterns.
    pub fn builtins() -> Self {
        Self::new(&BTreeMap::new(), &BTreeMap::new())
            .expect("built-in section patterns are valid regexes")
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
        wince::assert_eq!(
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
        wince::assert_eq!(
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
        wince::assert_eq!(
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
        wince::assert_eq!(
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
        let matchers = SectionMatchers::new(&overrides, &BTreeMap::new()).unwrap();
        let expected = "\
pub fn draw() { -> true
#[derive(Debug)] -> false
struct Theme { -> false
";
        // `struct` is no longer a definition since the replacement only knows
        // about `fn`, and the attribute line is excluded.
        wince::assert_eq!(
            classify(
                &matchers,
                "src/lib.rs",
                &["pub fn draw() {", "#[derive(Debug)]", "struct Theme {",]
            ),
            expected.to_string()
        );
    }

    /// Formats each attachment verdict as a `text -> bool` line.
    fn classify_attachment(matchers: &SectionMatchers, path: &str, lines: &[&str]) -> String {
        let section = matchers.for_path(path);
        let mut out = String::new();
        for line in lines {
            out.push_str(&format!("{} -> {}\n", line, section.is_attachment(line)));
        }
        out
    }

    #[test]
    fn rust_attaches_doc_comments_attributes_and_blanks_but_not_code() {
        // A doc or line comment, a block-comment continuation, an attribute,
        // and a blank line all lead into the definition below. A statement does
        // not.
        let matchers = SectionMatchers::builtins();
        let expected = "\
/// doc -> true
// note -> true
/* block -> true
 * cont -> true
#[inline] -> true
#![allow] -> true
 -> true
    let x = 1; -> false
pub fn draw() { -> false
";
        wince::assert_eq!(
            classify_attachment(
                &matchers,
                "src/lib.rs",
                &[
                    "/// doc",
                    "// note",
                    "/* block",
                    " * cont",
                    "#[inline]",
                    "#![allow]",
                    "",
                    "    let x = 1;",
                    "pub fn draw() {",
                ]
            ),
            expected.to_string()
        );
    }

    #[test]
    fn a_configured_attachment_replaces_the_builtin_prefixes_and_adds_languages() {
        // The configured list for rust replaces its built-in prefixes wholesale
        // rather than adding to them. Omitting `#[` from that list means an
        // attribute line no longer attaches. Go has no built-in prefixes to
        // replace. Its configured `//` is simply the only prefix it has.
        let attachment: BTreeMap<String, Vec<String>> = [
            ("rust".to_string(), vec!["//".to_string()]),
            ("go".to_string(), vec!["//".to_string()]),
        ]
        .into_iter()
        .collect();
        let matchers = SectionMatchers::new(&BTreeMap::new(), &attachment).unwrap();
        let rust = "\
/// doc -> true
#[inline] -> false
 -> true
";
        wince::assert_eq!(
            classify_attachment(&matchers, "src/lib.rs", &["/// doc", "#[inline]", ""]),
            rust.to_string()
        );
        let go = "\
// note -> true
x := 1 -> false
";
        wince::assert_eq!(
            classify_attachment(&matchers, "m.go", &["// note", "x := 1"]),
            go.to_string()
        );
    }

    #[test]
    fn an_unknown_language_attaches_only_blank_lines() {
        // Comment syntax is ambiguous without language rules. Only blank lines
        // may extend a leading block.
        let matchers = SectionMatchers::builtins();
        let expected = "\
# heading -> false
 -> true
text -> false
";
        wince::assert_eq!(
            classify_attachment(&matchers, "notes.txt", &["# heading", "", "text"]),
            expected.to_string()
        );
    }

    #[test]
    fn an_invalid_configured_pattern_is_an_error() {
        let overrides: BTreeMap<String, Vec<String>> =
            [("go".to_string(), vec!["(unclosed".to_string()])]
                .into_iter()
                .collect();
        let message = match SectionMatchers::new(&overrides, &BTreeMap::new()) {
            Ok(_) => panic!("expected an invalid-pattern error"),
            Err(error) => error.to_string(),
        };
        wince::assert_eq!(
            message,
            "invalid section pattern for go: Parsing error at position 9: Opening parenthesis without closing parenthesis".to_string()
        );
    }
}
