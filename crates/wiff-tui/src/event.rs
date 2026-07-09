//! Translating terminal key events into the binding [`KeyPress`] model.
//!
//! The event loop reads crossterm key events; the keymap is expressed against
//! wiff's own [`KeyPress`]. This module bridges the two at the edge so the rest
//! of the UI never sees a backend type. Keys with no wiff equivalent (lock keys,
//! media keys, raw modifier presses) translate to nothing and are ignored.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::key::{Key, KeyPress};

/// Translate a crossterm key event into a [`KeyPress`], or `None` for a key with
/// no binding equivalent or an event that is not a fresh press.
///
/// Release events are dropped so a binding fires once per press, not again on
/// key-up, on the terminals that report both.
pub fn to_key_press(event: KeyEvent) -> Option<KeyPress> {
    if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let key = to_key(event.code)?;
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);
    let shift = event.modifiers.contains(KeyModifiers::SHIFT);
    Some(KeyPress::with_modifiers(key, ctrl, alt, shift))
}

/// Map a crossterm key code to a wiff [`Key`], or `None` when there is none.
fn to_key(code: KeyCode) -> Option<Key> {
    Some(match code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::F(n) => Key::Function(n),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Escape,
        KeyCode::Tab => Key::Tab,
        // A back-tab is a shift-tab; it maps to Tab and keeps the reported
        // shift, so it binds the same as a keymap written "shift-tab".
        KeyCode::BackTab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::to_key_press;
    use crate::key::{Key, KeyPress};

    /// A press event for `code` with `modifiers`.
    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn translates_plain_named_and_modified_keys() {
        k9::assert_equal!(
            to_key_press(event(KeyCode::Char('j'), KeyModifiers::NONE)),
            Some(KeyPress::new(Key::Char('j')))
        );
        k9::assert_equal!(
            to_key_press(event(KeyCode::PageDown, KeyModifiers::NONE)),
            Some(KeyPress::new(Key::PageDown))
        );
        k9::assert_equal!(
            to_key_press(event(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Some(KeyPress::with_modifiers(Key::Char('f'), true, false, false))
        );
    }

    #[test]
    fn folds_a_shifted_letter_and_back_tab_into_their_bindings() {
        // A shifted "g" and a bare "G" both bind as uppercase with shift cleared.
        k9::assert_equal!(
            to_key_press(event(KeyCode::Char('g'), KeyModifiers::SHIFT)),
            Some(KeyPress::new(Key::Char('G')))
        );
        k9::assert_equal!(
            to_key_press(event(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(KeyPress::with_modifiers(Key::Tab, false, false, true))
        );
    }

    #[test]
    fn drops_releases_and_keys_with_no_binding_equivalent() {
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        k9::assert_equal!(to_key_press(release), None);
        k9::assert_equal!(
            to_key_press(event(KeyCode::CapsLock, KeyModifiers::NONE)),
            None
        );
        k9::assert_equal!(to_key_press(event(KeyCode::Null, KeyModifiers::NONE)), None);
    }
}
