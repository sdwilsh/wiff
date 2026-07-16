//! The wiff terminal UI: the [`action`] vocabulary the UI is driven by, the
//! [`key`] model bindings are expressed against, the [`keymap`] that resolves
//! key presses into actions, the [`theme`] palette, and the [`render`] layer
//! that paints a diff into styled terminal lines, the [`app`] that scrolls a
//! cursor over that rendering, and the [`run`] event loop that drives it in the
//! terminal.

pub mod action;
pub mod app;
pub mod compose;
mod editor_wrap;
pub mod event;
pub mod exit;
pub mod help;
pub mod highlight;
pub mod input;
pub mod key;
pub mod keymap;
mod markdown;
pub mod picker;
pub mod render;
pub mod review;
pub mod run;
pub mod search;
pub mod theme;
pub mod wrap;

pub use action::Action;
pub use app::{App, CompareRequest, Update};
pub use event::to_key_press;
pub use exit::{Exit, ExitDefault};
pub use highlight::BackgroundHighlighter;
pub use input::Input;
pub use key::{Chord, Key, KeyPress};
pub use keymap::{Keymap, KeymapError, KeymapOverrides, Resolution};
pub use render::{BoxId, DiffView, Document, Fold, KeyHints, ParsedFile, Row, RowKind};
pub use review::{CommentSync, Review};
pub use run::run;
pub use theme::Theme;
