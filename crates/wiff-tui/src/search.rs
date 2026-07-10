//! Incremental, `less`-style search over the rendered document.
//!
//! A [`Search`] is the live prompt: it collects the pattern as the reviewer
//! types and remembers the row the cursor started on, so an abandoned or empty
//! search returns there. The app drives the matching itself, since finding a row
//! and moving the cursor onto it needs the document and the fold and comment
//! collapse state the app owns.

use std::ops::Range;

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

/// Whether `pattern` occurs in `haystack`, honoring smart case: a pattern with
/// an uppercase letter matches case-sensitively, otherwise case-insensitively.
/// An empty pattern matches nothing.
pub fn matches(haystack: &str, pattern: &str) -> bool {
    !match_ranges(haystack, pattern).is_empty()
}

/// The byte ranges of every non-overlapping occurrence of `pattern` in
/// `haystack`, in order, honoring the same smart case as [`matches`]. Empty when
/// the pattern is empty or absent, so a caller can highlight each occurrence.
pub fn match_ranges(haystack: &str, pattern: &str) -> Vec<Range<usize>> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let sensitive = pattern.chars().any(char::is_uppercase);
    let pat: Vec<char> = pattern.chars().collect();
    let hay: Vec<(usize, char)> = haystack.char_indices().collect();
    let mut ranges = Vec::new();
    let mut start = 0;
    while start + pat.len() <= hay.len() {
        let hit = pat
            .iter()
            .enumerate()
            .all(|(offset, &want)| char_eq(hay[start + offset].1, want, sensitive));
        if hit {
            let from = hay[start].0;
            let to = hay
                .get(start + pat.len())
                .map_or(haystack.len(), |&(byte, _)| byte);
            ranges.push(from..to);
            start += pat.len();
        } else {
            start += 1;
        }
    }
    ranges
}

/// Compare one haystack character to one pattern character, case-sensitively or
/// folding case per the smart-case rule.
fn char_eq(a: char, b: char, sensitive: bool) -> bool {
    if sensitive {
        a == b
    } else {
        a.to_lowercase().eq(b.to_lowercase())
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
    use super::{Direction, match_ranges, matches};

    #[test]
    fn smart_case_matches_insensitively_until_the_pattern_has_an_uppercase() {
        // A lowercase pattern ignores case; adding an uppercase letter makes the
        // whole pattern case-sensitive, and an empty pattern never matches.
        k9::assert_equal!(matches("let Foo = 1;", "foo"), true);
        k9::assert_equal!(matches("let Foo = 1;", "Foo"), true);
        k9::assert_equal!(matches("let foo = 1;", "Foo"), false);
        k9::assert_equal!(matches("anything", ""), false);
    }

    #[test]
    fn match_ranges_locates_each_occurrence_honoring_smart_case() {
        // Every non-overlapping hit is reported in order; smart case folds a
        // lowercase pattern and pins an uppercase one, and a byte range spans
        // the matched characters even past a multibyte character.
        k9::assert_equal!(match_ranges("let l = let;", "let"), vec![0..3, 8..11]);
        k9::assert_equal!(match_ranges("Foo foo FOO", "foo"), vec![0..3, 4..7, 8..11]);
        k9::assert_equal!(match_ranges("Foo foo FOO", "Foo"), vec![0..3]);
        k9::assert_equal!(
            match_ranges("a\u{00e9}b a\u{00e9}b", "\u{00e9}b"),
            vec![1..4, 6..9]
        );
        k9::assert_equal!(
            match_ranges("anything", ""),
            Vec::<super::Range<usize>>::new()
        );
    }

    #[test]
    fn a_direction_reverses_and_names_its_prompt_lead() {
        k9::assert_equal!(Direction::Forward.reversed(), Direction::Backward);
        k9::assert_equal!(Direction::Backward.reversed(), Direction::Forward);
        k9::assert_equal!(Direction::Forward.lead(), '/');
        k9::assert_equal!(Direction::Backward.lead(), '?');
    }
}
