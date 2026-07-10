//! Incremental, `less`-style search over the rendered document.
//!
//! A [`Search`] is the live prompt: it collects the pattern as the reviewer
//! types and remembers the row the cursor started on, so an abandoned or empty
//! search returns there. The app drives the matching itself, since finding a row
//! and moving the cursor onto it needs the document and the fold and comment
//! collapse state the app owns.

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
    if pattern.is_empty() {
        return false;
    }
    if pattern.chars().any(char::is_uppercase) {
        haystack.contains(pattern)
    } else {
        haystack.to_lowercase().contains(&pattern.to_lowercase())
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
    use super::{Direction, matches};

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
    fn a_direction_reverses_and_names_its_prompt_lead() {
        k9::assert_equal!(Direction::Forward.reversed(), Direction::Backward);
        k9::assert_equal!(Direction::Backward.reversed(), Direction::Forward);
        k9::assert_equal!(Direction::Forward.lead(), '/');
        k9::assert_equal!(Direction::Backward.lead(), '?');
    }
}
