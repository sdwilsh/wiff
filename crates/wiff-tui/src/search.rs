//! Incremental, `less`-style search over the rendered document.
//!
//! A [`Search`] is the live prompt: it collects the pattern as the reviewer
//! types and remembers the row the cursor started on, so an abandoned or empty
//! search returns there. The app drives the matching itself, since finding a row
//! and moving the cursor onto it needs the document and the fold and comment
//! collapse state the app owns.

use std::ops::Range;

use fancy_regex::Regex;

use crate::key::{Key, KeyPress};

/// The direction a search scans and repeats in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Scanning down the document toward the end.
    Forward,
    /// Scanning up the document toward the start.
    Backward,
}

impl Direction {
    /// The opposite direction, for a reverse repeat.
    pub fn reversed(self) -> Self {
        match self {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        }
    }

    /// The character leading the prompt, matching `less`: `/` forward, `?` back.
    pub fn lead(self) -> char {
        match self {
            Direction::Forward => '/',
            Direction::Backward => '?',
        }
    }
}

/// A compiled search pattern: a regular expression honoring smart case, so a
/// caller can test many rows against one typed pattern without recompiling it.
pub struct Matcher {
    regex: Regex,
}

impl Matcher {
    /// Compile `pattern` as a regular expression, applying smart case: a pattern
    /// with an uppercase letter matches case-sensitively, otherwise it folds
    /// case. An empty pattern, or one that is not yet valid regex syntax (as a
    /// half-typed pattern often is), yields `None` so it simply matches nothing
    /// until it is well formed.
    pub fn new(pattern: &str) -> Option<Self> {
        if pattern.is_empty() {
            return None;
        }
        let folded = if pattern.chars().any(char::is_uppercase) {
            pattern.to_string()
        } else {
            format!("(?i){pattern}")
        };
        Regex::new(&folded).ok().map(|regex| Self { regex })
    }

    /// Whether the pattern occurs anywhere in `haystack`.
    pub fn is_match(&self, haystack: &str) -> bool {
        self.regex.is_match(haystack).unwrap_or(false)
    }

    /// The byte ranges of every non-overlapping occurrence of the pattern in
    /// `haystack`, in order, so a caller can highlight each one. Zero-width
    /// matches are dropped, since there is nothing to highlight for them.
    pub fn ranges(&self, haystack: &str) -> Vec<Range<usize>> {
        self.regex
            .find_iter(haystack)
            .filter_map(Result::ok)
            .map(|m| m.range())
            .filter(|range| !range.is_empty())
            .collect()
    }
}

/// What feeding a key to the prompt asks the app to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchInput {
    /// The pattern changed; re-run the incremental search from the origin.
    Edited,
    /// The reviewer accepted the pattern.
    Submit,
    /// The reviewer abandoned the prompt; return to the origin.
    Cancel,
    /// The key is not one the prompt consumes.
    Ignored,
}

/// A live search prompt: the direction it runs in, the pattern typed so far, and
/// the document row the cursor sat on when it opened.
pub struct Search {
    direction: Direction,
    pattern: String,
    origin: usize,
}

impl Search {
    /// A prompt scanning `direction`, returning to document row `origin` if it
    /// is cancelled or left empty.
    pub fn new(direction: Direction, origin: usize) -> Self {
        Self {
            direction,
            pattern: String::new(),
            origin,
        }
    }

    /// The direction the search scans.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The pattern typed so far.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The document row the cursor started on.
    pub fn origin(&self) -> usize {
        self.origin
    }

    /// The prompt as shown in the status line: the lead character then the
    /// pattern.
    pub fn prompt(&self) -> String {
        format!("{}{}", self.direction.lead(), self.pattern)
    }

    /// Feed a key to the prompt: Enter accepts, Escape abandons, Backspace edits
    /// (abandoning once the pattern is emptied), and a plain character extends
    /// the pattern.
    pub fn key(&mut self, press: KeyPress) -> SearchInput {
        match press.key {
            Key::Enter => SearchInput::Submit,
            Key::Escape => SearchInput::Cancel,
            Key::Backspace => {
                if self.pattern.pop().is_some() {
                    SearchInput::Edited
                } else {
                    SearchInput::Cancel
                }
            }
            Key::Char(c) if !press.ctrl && !press.alt => {
                self.pattern.push(c);
                SearchInput::Edited
            }
            _ => SearchInput::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Direction, Matcher};

    /// Match `pattern` against `haystack`, treating an empty or invalid pattern
    /// as one that matches nothing.
    fn is_match(haystack: &str, pattern: &str) -> bool {
        Matcher::new(pattern).is_some_and(|m| m.is_match(haystack))
    }

    /// The match ranges of `pattern` in `haystack`, empty for an empty or
    /// invalid pattern.
    fn ranges(haystack: &str, pattern: &str) -> Vec<super::Range<usize>> {
        Matcher::new(pattern)
            .map(|m| m.ranges(haystack))
            .unwrap_or_default()
    }

    #[test]
    fn smart_case_matches_insensitively_until_the_pattern_has_an_uppercase() {
        // A lowercase pattern ignores case; adding an uppercase letter makes the
        // whole pattern case-sensitive, and an empty pattern never matches.
        wince::assert_eq!(is_match("let Foo = 1;", "foo"), true);
        wince::assert_eq!(is_match("let Foo = 1;", "Foo"), true);
        wince::assert_eq!(is_match("let foo = 1;", "Foo"), false);
        wince::assert_eq!(is_match("anything", ""), false);
    }

    #[test]
    fn ranges_locate_each_occurrence_honoring_smart_case() {
        // Every non-overlapping hit is reported in order; smart case folds a
        // lowercase pattern and pins an uppercase one, and a byte range spans
        // the matched characters even past a multibyte character.
        wince::assert_eq!(ranges("let l = let;", "let"), vec![0..3, 8..11]);
        wince::assert_eq!(ranges("Foo foo FOO", "foo"), vec![0..3, 4..7, 8..11]);
        wince::assert_eq!(ranges("Foo foo FOO", "Foo"), vec![0..3]);
        wince::assert_eq!(
            ranges("a\u{00e9}b a\u{00e9}b", "\u{00e9}b"),
            vec![1..4, 6..9]
        );
        wince::assert_eq!(ranges("anything", ""), Vec::<super::Range<usize>>::new());
    }

    #[test]
    fn a_regular_expression_matches_and_locates_its_hits() {
        // The pattern is a regex, not a literal: character classes, anchors, and
        // alternation all apply, and an invalid pattern matches nothing.
        wince::assert_eq!(ranges("a1 b2 c3", "[a-z][0-9]"), vec![0..2, 3..5, 6..8]);
        wince::assert_eq!(ranges("foo bar foo", "^foo"), vec![0..3]);
        wince::assert_eq!(ranges("cat and dog", "cat|dog"), vec![0..3, 8..11]);
        wince::assert_eq!(is_match("anything", "(unclosed"), false);
    }

    #[test]
    fn a_direction_reverses_and_names_its_prompt_lead() {
        wince::assert_eq!(Direction::Forward.reversed(), Direction::Backward);
        wince::assert_eq!(Direction::Backward.reversed(), Direction::Forward);
        wince::assert_eq!(Direction::Forward.lead(), '/');
        wince::assert_eq!(Direction::Backward.lead(), '?');
    }
}
