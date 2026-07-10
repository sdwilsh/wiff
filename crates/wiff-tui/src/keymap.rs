//! Binding key chords to [`Action`]s.
//!
//! A [`Keymap`] resolves a sequence of key presses into an action. It is built
//! from the built-in defaults overlaid with the user's configured bindings, so a
//! user overrides only the actions they care about while the rest keep their
//! defaults. Overlaying an action with an empty chord list unbinds it, and
//! `disable_default_keymap` starts from nothing so only configured bindings
//! take effect.

use std::collections::{BTreeMap, HashMap};

use crate::action::Action;
use crate::key::{Chord, KeyPress};

/// A user's keymap overrides: a chord list per action, replacing that action's
/// default bindings. Absent actions keep their defaults; an empty list unbinds.
pub type KeymapOverrides = BTreeMap<Action, Vec<Chord>>;

/// The outcome of feeding the current pending presses to a [`Keymap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The presses complete a binding; run this action and clear the pending
    /// sequence.
    Action(Action),
    /// The presses are a prefix of one or more bindings; wait for more.
    Pending,
    /// The presses match no binding; clear the pending sequence.
    None,
}

/// An error building a keymap from bindings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    /// Two actions claim the same chord, so a press would be ambiguous.
    #[error("chord {chord:?} is bound to both {first} and {second}")]
    Conflict {
        /// The conflicting chord, rendered as its presses.
        chord: String,
        /// The action that claimed the chord first.
        first: &'static str,
        /// The action that also claimed it.
        second: &'static str,
    },
}

/// A resolved set of key bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    /// The chords bound to each action, kept for display and round-tripping.
    by_action: BTreeMap<Action, Vec<Chord>>,
    /// The reverse lookup used to resolve a completed chord to its action.
    by_chord: HashMap<Chord, Action>,
}

impl Keymap {
    /// The built-in default bindings, resembling `less` for navigation with the
    /// review actions layered on top.
    pub fn defaults() -> Self {
        // A pure data table; the expect is justified because the defaults are
        // known to be free of duplicate chords, which a test also guards.
        Self::from_action_map(default_bindings()).expect("default keymap has no conflicting chords")
    }

    /// Build the effective keymap: the defaults (unless `disable_defaults`)
    /// overlaid with the user's per-action overrides.
    pub fn resolve_config(
        overrides: &KeymapOverrides,
        disable_defaults: bool,
    ) -> Result<Self, KeymapError> {
        let mut by_action: BTreeMap<Action, Vec<Chord>> = if disable_defaults {
            BTreeMap::new()
        } else {
            default_bindings()
        };
        for (action, chords) in overrides {
            by_action.insert(*action, chords.clone());
        }
        by_action.retain(|_, chords| !chords.is_empty());
        Self::from_action_map(by_action)
    }

    /// Resolve the current pending presses: a completed action, a prefix of a
    /// longer binding, or no match.
    pub fn resolve(&self, pending: &[KeyPress]) -> Resolution {
        if pending.is_empty() {
            return Resolution::None;
        }
        if let Some(action) = self.by_chord.get(&Chord(pending.to_vec())) {
            return Resolution::Action(*action);
        }
        if self
            .by_chord
            .keys()
            .any(|Chord(presses)| presses.len() > pending.len() && presses.starts_with(pending))
        {
            return Resolution::Pending;
        }
        Resolution::None
    }

    /// The chords bound to `action`, in configured order.
    pub fn chords(&self, action: Action) -> &[Chord] {
        self.by_action.get(&action).map_or(&[], Vec::as_slice)
    }

    fn from_action_map(by_action: BTreeMap<Action, Vec<Chord>>) -> Result<Self, KeymapError> {
        let mut by_chord: HashMap<Chord, Action> = HashMap::new();
        for (action, chords) in &by_action {
            for chord in chords {
                if let Some(first) = by_chord.insert(chord.clone(), *action) {
                    return Err(KeymapError::Conflict {
                        chord: render_chord(chord),
                        first: first.name(),
                        second: action.name(),
                    });
                }
            }
        }
        Ok(Self {
            by_action,
            by_chord,
        })
    }
}

/// Render a chord for an error message, as space-separated presses.
fn render_chord(chord: &Chord) -> String {
    chord
        .0
        .iter()
        .map(|press| format!("{press:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse `text` into a chord, panicking on malformed input. Only used to build
/// the compile-time defaults, whose spellings are known-good.
fn chord(text: &str) -> Chord {
    text.parse().expect("a default binding is a valid chord")
}

/// The built-in bindings as an action-keyed map.
fn default_bindings() -> BTreeMap<Action, Vec<Chord>> {
    [
        (Action::LineDown, vec![chord("j"), chord("down")]),
        (Action::LineUp, vec![chord("k"), chord("up")]),
        (
            Action::PageDown,
            vec![chord("space"), chord("ctrl-f"), chord("pagedown")],
        ),
        (
            Action::PageUp,
            vec![chord("b"), chord("ctrl-b"), chord("pageup")],
        ),
        (Action::Top, vec![chord("g"), chord("<"), chord("home")]),
        (Action::Bottom, vec![chord("G"), chord(">"), chord("end")]),
        (Action::NextFile, vec![chord(".")]),
        (Action::PrevFile, vec![chord(",")]),
        (Action::NextHunk, vec![chord("]")]),
        (Action::PrevHunk, vec![chord("[")]),
        (Action::NextComment, vec![chord("}")]),
        (Action::PrevComment, vec![chord("{")]),
        (Action::ToggleFold, vec![chord("enter")]),
        (Action::ToggleComment, vec![chord("tab")]),
        (Action::AddComment, vec![chord("c")]),
        (Action::EditComment, vec![chord("e")]),
        (Action::ResolveComment, vec![chord("r")]),
        (Action::DeleteComment, vec![chord("d")]),
        (Action::Refresh, vec![chord("R")]),
        (Action::OpenInEditor, vec![chord("o")]),
        (Action::Quit, vec![chord("q")]),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{Action, Keymap, KeymapError, KeymapOverrides, Resolution};
    use crate::key::{Chord, KeyPress};

    fn presses(text: &str) -> Vec<KeyPress> {
        let Chord(presses) = text.parse().unwrap();
        presses
    }

    /// Build a keymap from per-action overrides given as `(action, chords)`.
    fn build(pairs: &[(Action, &[&str])], disable_defaults: bool) -> Result<Keymap, KeymapError> {
        let overrides: KeymapOverrides = pairs
            .iter()
            .map(|(action, chords)| (*action, chords.iter().map(|c| c.parse().unwrap()).collect()))
            .collect();
        Keymap::resolve_config(&overrides, disable_defaults)
    }

    #[test]
    fn the_defaults_resolve_navigation_and_review_keys() {
        let map = Keymap::defaults();
        k9::assert_equal!(
            map.resolve(&presses("j")),
            Resolution::Action(Action::LineDown)
        );
        k9::assert_equal!(
            map.resolve(&presses("down")),
            Resolution::Action(Action::LineDown)
        );
        k9::assert_equal!(
            map.resolve(&presses("space")),
            Resolution::Action(Action::PageDown)
        );
        k9::assert_equal!(
            map.resolve(&presses("ctrl-f")),
            Resolution::Action(Action::PageDown)
        );
        k9::assert_equal!(
            map.resolve(&presses("G")),
            Resolution::Action(Action::Bottom)
        );
        // less-style angle brackets also jump to the ends of the diff.
        k9::assert_equal!(map.resolve(&presses("<")), Resolution::Action(Action::Top));
        k9::assert_equal!(
            map.resolve(&presses(">")),
            Resolution::Action(Action::Bottom)
        );
        k9::assert_equal!(
            map.resolve(&presses("c")),
            Resolution::Action(Action::AddComment)
        );
        k9::assert_equal!(
            map.resolve(&presses("R")),
            Resolution::Action(Action::Refresh)
        );
        k9::assert_equal!(map.resolve(&presses("q")), Resolution::Action(Action::Quit));
        k9::assert_equal!(map.resolve(&presses("z")), Resolution::None);
    }

    #[test]
    fn an_override_replaces_only_that_actions_bindings() {
        let map = build(&[(Action::LineDown, &["x"])], false).unwrap();
        // The override takes effect and the default "j" no longer binds, but
        // untouched actions keep their defaults.
        k9::assert_equal!(
            map.resolve(&presses("x")),
            Resolution::Action(Action::LineDown)
        );
        k9::assert_equal!(map.resolve(&presses("j")), Resolution::None);
        k9::assert_equal!(
            map.resolve(&presses("k")),
            Resolution::Action(Action::LineUp)
        );
        k9::assert_equal!(
            map.chords(Action::LineDown),
            &["x".parse::<Chord>().unwrap()]
        );
    }

    #[test]
    fn an_empty_override_unbinds_an_action() {
        let map = build(&[(Action::Quit, &[])], false).unwrap();
        k9::assert_equal!(map.resolve(&presses("q")), Resolution::None);
        k9::assert_equal!(map.chords(Action::Quit), &[] as &[Chord]);
    }

    #[test]
    fn disabling_the_defaults_keeps_only_configured_bindings() {
        let map = build(&[(Action::Quit, &["x"])], true).unwrap();
        k9::assert_equal!(map.resolve(&presses("x")), Resolution::Action(Action::Quit));
        k9::assert_equal!(map.resolve(&presses("j")), Resolution::None);
        k9::assert_equal!(map.resolve(&presses("q")), Resolution::None);
    }

    #[test]
    fn a_chord_shared_by_two_actions_is_a_conflict() {
        let error = build(&[(Action::LineUp, &["j"])], false).unwrap_err();
        k9::assert_equal!(
            error,
            KeymapError::Conflict {
                chord: "KeyPress { key: Char('j'), ctrl: false, alt: false, shift: false }"
                    .to_string(),
                first: "line_down",
                second: "line_up",
            }
        );
    }

    #[test]
    fn a_multi_press_chord_reports_pending_until_complete() {
        let map = build(&[(Action::Top, &["g g"])], true).unwrap();
        k9::assert_equal!(map.resolve(&presses("g")), Resolution::Pending);
        k9::assert_equal!(
            map.resolve(&presses("g g")),
            Resolution::Action(Action::Top)
        );
        k9::assert_equal!(map.resolve(&presses("g x")), Resolution::None);
    }
}
