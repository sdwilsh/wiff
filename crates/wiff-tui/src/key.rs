//! A self-contained key model for bindings.
//!
//! The keymap is expressed against these types rather than a terminal backend's
//! event types, so bindings parse and compare without pulling in a backend and
//! the tests need no terminal. The event loop converts backend key events into
//! [`KeyPress`] at its edge.
//!
//! A [`Chord`] is a sequence of key presses (a single press for most bindings,
//! a sequence like `g g` for leader-style bindings). Each press is a [`Key`]
//! with optional `ctrl`/`alt`/`shift` modifiers, written as dash-joined prefixes
//! (`ctrl-f`, `shift-tab`), with presses separated by whitespace (`g g`).

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

/// A key on the keyboard, either a character or a named non-printing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A character key, carrying its case as typed.
    Char(char),
    /// A function key, `F1` through `F12`.
    Function(u8),
    /// The Enter/Return key.
    Enter,
    /// The Escape key.
    Escape,
    /// The Tab key.
    Tab,
    /// The Backspace key.
    Backspace,
    /// The Delete key.
    Delete,
    /// The Insert key.
    Insert,
    /// The Left arrow key.
    Left,
    /// The Right arrow key.
    Right,
    /// The Up arrow key.
    Up,
    /// The Down arrow key.
    Down,
    /// The Home key.
    Home,
    /// The End key.
    End,
    /// The Page Up key.
    PageUp,
    /// The Page Down key.
    PageDown,
}

/// A single key press: a [`Key`] with its active modifiers.
///
/// Modifiers on a character are normalized into the character's case: a press
/// of shifted `g` is `Char('G')` with `shift` cleared, so a binding written as
/// `G` and one written as `shift-g` compare equal, matching how terminals report
/// shifted letters inconsistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyPress {
    /// The key pressed.
    pub key: Key,
    /// Whether Control was held.
    pub ctrl: bool,
    /// Whether Alt was held.
    pub alt: bool,
    /// Whether Shift was held (never set for a character; see the type docs).
    pub shift: bool,
}

impl KeyPress {
    /// A press of `key` with no modifiers.
    pub fn new(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
        }
        .normalized()
    }

    /// Fold a shift modifier on a letter into the character's case, so the two
    /// ways of writing a shifted letter compare equal.
    fn normalized(mut self) -> Self {
        if let Key::Char(c) = self.key
            && self.shift
        {
            // A shifted letter is its uppercase form; a shifted symbol is
            // reported by its own value, so only letters fold.
            if c.is_ascii_alphabetic() {
                self.key = Key::Char(c.to_ascii_uppercase());
            }
            self.shift = false;
        }
        self
    }
}

/// A sequence of key presses that triggers an action.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chord(pub Vec<KeyPress>);

impl FromStr for Key {
    type Err = String;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        // A lone character is that character; longer tokens are named keys.
        let mut chars = token.chars();
        if let (Some(c), None) = (chars.next(), chars.clone().next()) {
            return Ok(Key::Char(c));
        }
        let lower = token.to_ascii_lowercase();
        if let Some(number) = lower.strip_prefix('f')
            && let Ok(n) = number.parse::<u8>()
            && (1..=12).contains(&n)
        {
            return Ok(Key::Function(n));
        }
        Ok(match lower.as_str() {
            "space" => Key::Char(' '),
            "enter" | "return" => Key::Enter,
            "esc" | "escape" => Key::Escape,
            "tab" => Key::Tab,
            "backspace" => Key::Backspace,
            "delete" | "del" => Key::Delete,
            "insert" | "ins" => Key::Insert,
            "left" => Key::Left,
            "right" => Key::Right,
            "up" => Key::Up,
            "down" => Key::Down,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" | "pgup" => Key::PageUp,
            "pagedown" | "pgdn" => Key::PageDown,
            other => return Err(format!("unknown key {other:?}")),
        })
    }
}

impl FromStr for KeyPress {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        // Strip known modifier prefixes from the front; whatever remains is the
        // key. Consuming a prefix rather than splitting on every dash keeps a
        // lone "-" (and "ctrl--") reading as the "-" key.
        let mut rest = text;
        loop {
            let lower = rest.to_ascii_lowercase();
            if let Some(after) = lower
                .strip_prefix("ctrl-")
                .or(lower.strip_prefix("control-"))
            {
                ctrl = true;
                rest = &rest[rest.len() - after.len()..];
            } else if let Some(after) = lower.strip_prefix("alt-").or(lower.strip_prefix("meta-")) {
                alt = true;
                rest = &rest[rest.len() - after.len()..];
            } else if let Some(after) = lower.strip_prefix("shift-") {
                shift = true;
                rest = &rest[rest.len() - after.len()..];
            } else {
                break;
            }
        }
        let key = rest.parse()?;
        Ok(KeyPress {
            key,
            ctrl,
            alt,
            shift,
        }
        .normalized())
    }
}

impl FromStr for Chord {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let presses = text
            .split_whitespace()
            .map(KeyPress::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        if presses.is_empty() {
            return Err("a chord must contain at least one key press".to_string());
        }
        Ok(Chord(presses))
    }
}

impl<'de> Deserialize<'de> for Chord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ChordVisitor;

        impl Visitor<'_> for ChordVisitor {
            type Value = Chord;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a key chord such as \"q\", \"ctrl-f\", or \"g g\"")
            }

            fn visit_str<E>(self, value: &str) -> Result<Chord, E>
            where
                E: de::Error,
            {
                value.parse().map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(ChordVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{Chord, Key, KeyPress};

    fn press(text: &str) -> KeyPress {
        text.parse().unwrap()
    }

    #[test]
    fn parses_plain_and_named_keys() {
        k9::assert_equal!(press("j"), KeyPress::new(Key::Char('j')));
        k9::assert_equal!(press("space"), KeyPress::new(Key::Char(' ')));
        k9::assert_equal!(press("enter"), KeyPress::new(Key::Enter));
        k9::assert_equal!(press("pagedown"), KeyPress::new(Key::PageDown));
        k9::assert_equal!(press("f5"), KeyPress::new(Key::Function(5)));
    }

    #[test]
    fn parses_modifier_prefixes() {
        k9::assert_equal!(
            press("ctrl-f"),
            KeyPress {
                key: Key::Char('f'),
                ctrl: true,
                alt: false,
                shift: false,
            }
        );
        k9::assert_equal!(
            press("alt-enter"),
            KeyPress {
                key: Key::Enter,
                ctrl: false,
                alt: true,
                shift: false,
            }
        );
    }

    #[test]
    fn folds_a_shifted_letter_into_its_case() {
        // "G", "shift-g", and a shifted "g" are the same binding.
        let expected = KeyPress::new(Key::Char('G'));
        k9::assert_equal!(press("G"), expected);
        k9::assert_equal!(press("shift-g"), expected);
        k9::assert_equal!(
            KeyPress {
                key: Key::Char('g'),
                ctrl: false,
                alt: false,
                shift: true,
            }
            .normalized(),
            expected
        );
    }

    #[test]
    fn parses_the_dash_key_and_multi_press_chords() {
        k9::assert_equal!(press("-"), KeyPress::new(Key::Char('-')));
        k9::assert_equal!(
            press("ctrl--"),
            KeyPress {
                key: Key::Char('-'),
                ctrl: true,
                alt: false,
                shift: false,
            }
        );
        let chord: Chord = "g g".parse().unwrap();
        k9::assert_equal!(
            chord,
            Chord(vec![
                KeyPress::new(Key::Char('g')),
                KeyPress::new(Key::Char('g')),
            ])
        );
    }

    #[test]
    fn rejects_unknown_keys_modifiers_and_empty_chords() {
        k9::assert_equal!(
            "nope".parse::<KeyPress>(),
            Err("unknown key \"nope\"".to_string())
        );
        k9::assert_equal!(
            "hyper-x".parse::<KeyPress>(),
            Err("unknown key \"hyper-x\"".to_string())
        );
        k9::assert_equal!(
            "   ".parse::<Chord>(),
            Err("a chord must contain at least one key press".to_string())
        );
    }
}
