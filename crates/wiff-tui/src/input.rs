//! Accumulating key presses into resolved actions.
//!
//! A binding may be a multi-press chord, so a press is not resolved in
//! isolation: it is appended to the pending sequence, which the [`Keymap`] then
//! reads. A completed binding yields its action and clears the sequence; a
//! prefix of a longer binding leaves the sequence pending for the next press; a
//! press that matches nothing clears the sequence so the next press starts
//! fresh.

use crate::action::Action;
use crate::key::KeyPress;
use crate::keymap::{Keymap, Resolution};

/// The pending-press state between a keymap and the event loop.
pub struct Input {
    keymap: Keymap,
    pending: Vec<KeyPress>,
}

impl Input {
    /// Build a dispatcher over `keymap` with no pending presses.
    pub fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            pending: Vec::new(),
        }
    }

    /// Feed one press, returning the action it completes, if any.
    pub fn press(&mut self, press: KeyPress) -> Option<Action> {
        self.pending.push(press);
        match self.keymap.resolve(&self.pending) {
            Resolution::Action(action) => {
                self.pending.clear();
                Some(action)
            }
            Resolution::Pending => None,
            Resolution::None => {
                self.pending.clear();
                None
            }
        }
    }

    /// Whether a partial chord is waiting for its next press.
    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Input;
    use crate::action::Action;
    use crate::key::{Chord, KeyPress};
    use crate::keymap::{Keymap, KeymapOverrides};

    fn press(text: &str) -> KeyPress {
        text.parse().expect("a valid single press")
    }

    /// A dispatcher over the given `(action, chords)` overrides with the
    /// defaults disabled, so only the listed bindings resolve.
    fn dispatcher(pairs: &[(Action, &[&str])]) -> Input {
        let overrides: KeymapOverrides = pairs
            .iter()
            .map(|(action, chords)| {
                (
                    *action,
                    chords
                        .iter()
                        .map(|c| c.parse::<Chord>().expect("a valid chord"))
                        .collect(),
                )
            })
            .collect();
        Input::new(Keymap::resolve_config(&overrides, true).expect("no conflicts"))
    }

    #[test]
    fn a_single_press_resolves_its_action_immediately() {
        let mut input = dispatcher(&[(Action::LineDown, &["j"])]);
        k9::assert_equal!(input.press(press("j")), Some(Action::LineDown));
        k9::assert_equal!(input.is_pending(), false);
    }

    #[test]
    fn a_multi_press_chord_waits_then_resolves() {
        let mut input = dispatcher(&[(Action::Top, &["g g"])]);
        k9::assert_equal!(input.press(press("g")), None);
        k9::assert_equal!(input.is_pending(), true);
        k9::assert_equal!(input.press(press("g")), Some(Action::Top));
        k9::assert_equal!(input.is_pending(), false);
    }

    #[test]
    fn an_unmatched_press_clears_the_pending_sequence() {
        let mut input = dispatcher(&[(Action::Top, &["g g"])]);
        k9::assert_equal!(input.press(press("g")), None);
        // "g x" matches no binding, so the sequence resets.
        k9::assert_equal!(input.press(press("x")), None);
        k9::assert_equal!(input.is_pending(), false);
    }
}
