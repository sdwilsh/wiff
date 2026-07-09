//! The wiff terminal UI: the [`action`] vocabulary the UI is driven by, the
//! [`key`] model bindings are expressed against, the [`keymap`] that resolves
//! key presses into actions, and (in later work) the ratatui rendering layer.

pub mod action;
pub mod key;
pub mod keymap;

pub use action::Action;
pub use key::{Chord, Key, KeyPress};
pub use keymap::{Keymap, KeymapError, KeymapOverrides, Resolution};
