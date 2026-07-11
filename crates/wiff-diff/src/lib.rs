//! The wiff diff library: the parsed unified-diff [`model`], a [`parse`]r from
//! diff text into that model, [`reconstitute`]ing one side's content from a
//! file's hunks, [`highlight`]ing that content with syntect, and [`intraline`]
//! refinement of changed lines. This crate is pure diff machinery with no
//! session or IO state.

pub mod highlight;
pub mod intraline;
pub mod line;
pub mod model;
pub mod parse;
pub mod reconstitute;
pub mod section;

pub use highlight::{
    HighlightError, HighlightedLine, Highlighter, ParsedSide, Parser, Rgb, Style, StyledSpan,
    ThemeChrome, fence_language, theme_chrome, theme_names,
};
pub use intraline::refine;
pub use line::LineNo;
pub use model::{Diff, DiffLine, FileDiff, FileStatus, Hunk, LineKind, Side};
pub use parse::{ParseError, parse};
pub use reconstitute::{ReconLine, known_lines, reconstitute};
pub use section::{Section, SectionError, SectionMatchers};
