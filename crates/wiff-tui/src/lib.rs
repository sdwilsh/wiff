//! The wiff terminal UI: the [`action`] vocabulary the UI is driven by, the
//! [`key`] model bindings are expressed against, the [`keymap`] that resolves
//! key presses into actions, the [`theme`] palette, and the [`render`] layer
//! that paints a diff into styled terminal lines.

pub mod action;
pub mod key;
pub mod keymap;
pub mod render;
pub mod theme;

pub use action::Action;
pub use key::{Chord, Key, KeyPress};
pub use keymap::{Keymap, KeymapError, KeymapOverrides, Resolution};
pub use render::DiffView;
pub use theme::Theme;
