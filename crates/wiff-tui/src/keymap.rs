//! Binding key chords to [`Action`]s.
//!
//! A [`Keymap`] resolves a sequence of key presses into an action. It is built
//! from the built-in defaults overlaid with the user's configured bindings, so a
//! user overrides only the actions they care about while the rest keep their
//! defaults. Overlaying an action with an empty chord list unbinds it, and
//! `disable_default_keymap` starts from nothing so only configured bindings
//! take effect.

use std::collections::{BTreeMap, HashMap};

use crate::action::{Action, Scope};
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
    /// The reverse lookup for each single [`Scope`], resolving a completed chord
    /// to its action within that context. A chord may appear in several of
    /// these tables bound to a different action in each.
    by_scope: HashMap<Scope, HashMap<Chord, Action>>,
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

    /// Resolve the current pending presses within `scope`, which names a
    /// context such as [`Scope::REVIEW`]: a completed action, a prefix of a
    /// longer binding, or no match. Takes exactly one scope flag. A combined
    /// scope such as [`Scope::DEFAULT`] doesn't name a context and resolves
    /// nothing.
    pub fn resolve(&self, pending: &[KeyPress], scope: Scope) -> Resolution {
        debug_assert_eq!(
            scope.iter().count(),
            1,
            "resolve takes exactly one scope flag"
        );
        if pending.is_empty() {
            return Resolution::None;
        }
        let Some(by_chord) = self.by_scope.get(&scope) else {
            return Resolution::None;
        };
        if let Some(action) = by_chord.get(&Chord(pending.to_vec())) {
            return Resolution::Action(*action);
        }
        if by_chord
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

    /// The first chord bound to `action`, rendered as a hint label, or `None`
    /// when the action has no binding.
    pub fn primary_label(&self, action: Action) -> Option<String> {
        self.chords(action).first().map(ToString::to_string)
    }

    fn from_action_map(by_action: BTreeMap<Action, Vec<Chord>>) -> Result<Self, KeymapError> {
        let mut by_scope: HashMap<Scope, HashMap<Chord, Action>> = HashMap::new();
        for (action, chords) in &by_action {
            for context in action.scope().iter() {
                let table = by_scope.entry(context).or_default();
                for chord in chords {
                    insert_chord(table, chord, *action)?;
                }
            }
        }
        Ok(Self {
            by_action,
            by_scope,
        })
    }
}

/// Record `chord` as bound to `action` in one scope's reverse lookup, failing
/// when another action already claimed it within that scope.
fn insert_chord(
    by_chord: &mut HashMap<Chord, Action>,
    chord: &Chord,
    action: Action,
) -> Result<(), KeymapError> {
    if let Some(first) = by_chord.insert(chord.clone(), action) {
        return Err(KeymapError::Conflict {
            chord: chord.to_string(),
            first: first.name(),
            second: action.name(),
        });
    }
    Ok(())
}

/// Parse `text` into a chord, panicking on malformed input. Only used to build
/// the compile-time defaults, whose spellings are known-good.
fn chord(text: &str) -> Chord {
    text.parse().expect("a default binding is a valid chord")
}

/// The built-in bindings as an action-keyed map.
fn default_bindings() -> BTreeMap<Action, Vec<Chord>> {
    [
        (Action::LineDown, vec![chord("down"), chord("j")]),
        (Action::LineUp, vec![chord("up"), chord("k")]),
        (
            Action::PageDown,
            vec![chord("space"), chord("ctrl-f"), chord("pagedown")],
        ),
        (
            Action::PageUp,
            vec![chord("b"), chord("ctrl-b"), chord("pageup")],
        ),
        (Action::PageDownHalf, vec![chord("ctrl-d")]),
        (Action::PageUpHalf, vec![chord("ctrl-u")]),
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
        (Action::ToggleWrap, vec![chord("w")]),
        (Action::ToggleLineNumbers, vec![chord("L")]),
        (Action::DiffModeAuto, vec![chord("0")]),
        (Action::DiffModeUnified, vec![chord("1")]),
        (Action::DiffModeSideBySide, vec![chord("2")]),
        (Action::DiffModeOnlyAfter, vec![chord("3")]),
        (Action::DiffModeRendered, vec![chord("4")]),
        (Action::HideComments, vec![chord("H")]),
        (Action::PickFile, vec![chord("t")]),
        (Action::PickComment, vec![chord("C")]),
        (Action::PickTheme, vec![chord("T")]),
        (Action::SelectLines, vec![chord("v")]),
        (Action::AddComment, vec![chord("c")]),
        (Action::ReplyComment, vec![chord("r")]),
        (Action::EditComment, vec![chord("e")]),
        (Action::ResolveComment, vec![chord("x")]),
        (Action::DeleteComment, vec![chord("d")]),
        (Action::SetVerdict, vec![chord("a")]),
        (Action::SubmitComment, vec![chord("ctrl-d")]),
        (Action::CancelComment, vec![chord("esc")]),
        (Action::DetachEditor, vec![chord("ctrl-o")]),
        (Action::Save, vec![chord("ctrl-s")]),
        (Action::SearchForward, vec![chord("/")]),
        (Action::SearchBackward, vec![chord("?")]),
        (Action::SearchNext, vec![chord("n")]),
        (Action::SearchPrev, vec![chord("N")]),
        (Action::Refresh, vec![chord("ctrl-r")]),
        (Action::Publish, vec![chord("P")]),
        (Action::AddFile, vec![chord("A")]),
        (Action::CompareVersions, vec![chord("V")]),
        (Action::OpenInEditor, vec![chord("o")]),
        (Action::Suspend, vec![chord("ctrl-z")]),
        (Action::Help, vec![chord("h")]),
        (Action::Quit, vec![chord("q")]),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{Action, Keymap, KeymapError, KeymapOverrides, Resolution, Scope};
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
    fn the_defaults_resolve_ctrl_d_in_each_scope() {
        // The one chord this change exists for: ctrl-d jumps a half page in the
        // review view and submits the comment in the editor, without the two
        // colliding. esc, however, binds the same action in both scopes.
        let map = Keymap::defaults();
        let review = |text: &str| map.resolve(&presses(text), Scope::REVIEW);
        wince::assert_eq!(review("ctrl-d"), Resolution::Action(Action::PageDownHalf));
        wince::assert_eq!(review("ctrl-u"), Resolution::Action(Action::PageUpHalf));
        wince::assert_eq!(review("esc"), Resolution::Action(Action::CancelComment));

        let editor = |text: &str| map.resolve(&presses(text), Scope::EDITOR);
        wince::assert_eq!(editor("ctrl-d"), Resolution::Action(Action::SubmitComment));
        wince::assert_eq!(editor("ctrl-u"), Resolution::None);
        wince::assert_eq!(editor("esc"), Resolution::Action(Action::CancelComment));
    }

    #[test]
    fn an_override_replaces_only_that_actions_bindings() {
        let map = build(&[(Action::LineDown, &["R"])], false).unwrap();
        // The override takes effect and the default "j" no longer binds, but
        // untouched actions keep their defaults.
        wince::assert_eq!(
            map.resolve(&presses("R"), Scope::REVIEW),
            Resolution::Action(Action::LineDown)
        );
        wince::assert_eq!(map.resolve(&presses("j"), Scope::REVIEW), Resolution::None);
        wince::assert_eq!(
            map.resolve(&presses("k"), Scope::REVIEW),
            Resolution::Action(Action::LineUp)
        );
        wince::assert_eq!(
            map.chords(Action::LineDown),
            &["R".parse::<Chord>().unwrap()]
        );
    }

    #[test]
    fn an_empty_override_unbinds_an_action() {
        let map = build(&[(Action::Quit, &[])], false).unwrap();
        wince::assert_eq!(map.resolve(&presses("q"), Scope::REVIEW), Resolution::None);
        wince::assert_eq!(map.chords(Action::Quit), &[] as &[Chord]);
    }

    #[test]
    fn disabling_the_defaults_keeps_only_configured_bindings() {
        let map = build(&[(Action::Quit, &["x"])], true).unwrap();
        wince::assert_eq!(
            map.resolve(&presses("x"), Scope::REVIEW),
            Resolution::Action(Action::Quit)
        );
        wince::assert_eq!(map.resolve(&presses("j"), Scope::REVIEW), Resolution::None);
        wince::assert_eq!(map.resolve(&presses("q"), Scope::REVIEW), Resolution::None);
    }

    #[test]
    fn a_chord_shared_by_two_actions_is_a_conflict() {
        let error = build(&[(Action::LineUp, &["j"])], false).unwrap_err();
        wince::assert_eq!(
            error,
            KeymapError::Conflict {
                chord: "j".to_string(),
                first: "line_down",
                second: "line_up",
            }
        );
    }

    #[test]
    fn a_multi_press_chord_reports_pending_until_complete() {
        let map = build(&[(Action::Top, &["g g"])], true).unwrap();
        wince::assert_eq!(
            map.resolve(&presses("g"), Scope::REVIEW),
            Resolution::Pending
        );
        wince::assert_eq!(
            map.resolve(&presses("g g"), Scope::REVIEW),
            Resolution::Action(Action::Top)
        );
        wince::assert_eq!(
            map.resolve(&presses("g x"), Scope::REVIEW),
            Resolution::None
        );
    }
}
