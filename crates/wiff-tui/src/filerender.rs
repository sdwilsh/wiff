//! Selecting a whole-file renderer for a file from its path.
//!
//! The rendered review layout needs to know which files it can present as
//! formatted output rather than source, and how. This module makes that choice
//! and leaves the rendering itself to the caller that holds the theme and syntax
//! highlighter. A new format is added here as another renderer without touching
//! the layout code.

use std::path::Path;

/// A whole-file renderer, selected from a file's path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileRenderer {
    /// GitHub-flavored markdown.
    Markdown,
}

impl FileRenderer {
    /// The renderer for `path`, or `None` when no renderer fits its type.
    pub fn for_path(path: &str) -> Option<Self> {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" => Some(FileRenderer::Markdown),
            _ => None,
        }
    }
}
