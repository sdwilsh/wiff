//! The colors the diff renderer paints with.
//!
//! Syntax coloring of the file content comes from a syntect theme, named here
//! and resolved by the renderer. Everything the renderer decides itself -- the
//! line-number gutter, the tint behind added and removed rows, and the darker
//! tint marking the characters that actually changed within a row -- is a color
//! on the [`Theme`], so the whole palette lives in one place.

use wiff_diff::Rgb;

/// The palette for rendering a diff: the syntect theme for content coloring
/// plus the backgrounds and accents the renderer applies around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// The syntect theme name used to color file content.
    pub syntax_theme: String,
    /// The gutter text color (line numbers and change markers).
    pub gutter_fg: Rgb,
    /// The file-header text color.
    pub file_header_fg: Rgb,
    /// The hunk-header text color.
    pub hunk_header_fg: Rgb,
    /// The background behind an added row.
    pub added_bg: Rgb,
    /// The background behind a removed row.
    pub removed_bg: Rgb,
    /// The background behind the changed characters of an added row.
    pub added_emphasis_bg: Rgb,
    /// The background behind the changed characters of a removed row.
    pub removed_emphasis_bg: Rgb,
    /// The background marking trailing whitespace on an added row.
    pub whitespace_bg: Rgb,
    /// The background washed over the row the cursor is on.
    pub cursor_bg: Rgb,
    /// The text color of a fold marker standing in for hidden unchanged lines.
    pub fold_fg: Rgb,
    /// The text color of the status line at the bottom of the screen.
    pub status_fg: Rgb,
    /// The background of the status line at the bottom of the screen.
    pub status_bg: Rgb,
    /// The heading color of the review summary row at the top of the document.
    pub review_fg: Rgb,
    /// The body text color of a comment.
    pub comment_fg: Rgb,
    /// The color of a comment's author attribution.
    pub comment_author_fg: Rgb,
    /// The color of a muted comment badge such as `resolved`.
    pub comment_flag_fg: Rgb,
    /// The color of a warning comment badge such as `shifted` or `outdated`.
    pub comment_warn_fg: Rgb,
    /// The color of the `draft` badge marking a comment with uncommitted edits.
    pub comment_draft_fg: Rgb,
}

impl Theme {
    /// The default palette for a dark terminal, matched to the default dark
    /// syntect theme.
    pub fn dark() -> Self {
        Self {
            syntax_theme: wiff_diff::highlight::DEFAULT_DARK_THEME.to_string(),
            gutter_fg: rgb(0x65, 0x73, 0x7e),
            file_header_fg: rgb(0xc0, 0xc5, 0xce),
            hunk_header_fg: rgb(0x96, 0xb5, 0xb4),
            added_bg: rgb(0x2d, 0x3b, 0x30),
            removed_bg: rgb(0x3b, 0x2d, 0x30),
            added_emphasis_bg: rgb(0x3a, 0x5a, 0x40),
            removed_emphasis_bg: rgb(0x5a, 0x3a, 0x40),
            whitespace_bg: rgb(0x9a, 0x2a, 0x2a),
            cursor_bg: rgb(0x4f, 0x5b, 0x66),
            fold_fg: rgb(0x8a, 0x8a, 0x8a),
            status_fg: rgb(0xc0, 0xc5, 0xce),
            status_bg: rgb(0x34, 0x3d, 0x46),
            review_fg: rgb(0xeb, 0xcb, 0x8b),
            comment_fg: rgb(0xc0, 0xc5, 0xce),
            comment_author_fg: rgb(0x8f, 0xa1, 0xb3),
            comment_flag_fg: rgb(0x8a, 0x8a, 0x8a),
            comment_warn_fg: rgb(0xd0, 0x87, 0x70),
            comment_draft_fg: rgb(0xa3, 0xbe, 0x8c),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// A concise [`Rgb`] constructor for the palette table.
const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}
