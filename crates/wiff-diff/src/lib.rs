//! The wiff diff library: the parsed unified-diff [`model`], a [`parse`]r from
//! diff text into that model, [`reconstitute`]ing one side's content from a
//! file's hunks, [`highlight`]ing that content with syntect, and [`intraline`]
//! refinement of changed lines, [`tabs`] expansion for fixed-column display,
//! and [`generated_files`] recognition for collapsing machine-written files.
//! This crate is pure diff machinery with no session or IO state.

pub mod content;
pub mod generated_files;
pub mod highlight;
pub mod intraline;
pub mod line;
pub mod model;
pub mod parse;
pub mod reconstitute;
pub mod section;
pub mod tabs;

pub use content::decode_text;
pub use generated_files::{GeneratedError, GeneratedMatchers, GeneratedReason};
pub use highlight::{
    HighlightError, HighlightedLine, Highlighter, LiveHighlighter, ParsedSide, Parser, Rgb, Style,
    StyledSpan, ThemeChrome, ThemeName, UnknownTheme, fence_language, theme_chrome, theme_names,
};
pub use intraline::refine;
pub use line::LineNo;
pub use model::{Diff, DiffLine, FileDiff, FileStatus, Hunk, LineKind, Side};
pub use parse::{ParseError, parse};
pub use reconstitute::{ReconLine, known_lines, reconstitute};
pub use section::{Section, SectionError, SectionMatchers};
pub use tabs::{DEFAULT_TAB_WIDTH, expand_tabs};
