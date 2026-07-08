//! The wiff diff library: the parsed unified-diff [`model`], a [`parse`]r from
//! diff text into that model, and [`reconstitute`]ing one side's content from a
//! file's hunks. This crate is pure diff machinery with no session or IO state.

pub mod line;
pub mod model;
pub mod parse;
pub mod reconstitute;

pub use line::LineNo;
pub use model::{Diff, DiffLine, FileDiff, FileStatus, Hunk, LineKind, Side};
pub use parse::{ParseError, parse};
pub use reconstitute::{ReconLine, known_lines, reconstitute};
