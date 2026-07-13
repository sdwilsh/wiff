//! The colors the diff renderer paints with.
//!
//! Syntax coloring of the file content comes from a syntect theme, named here
//! and resolved by the renderer. The rest of the palette -- the background the
//! whole view fills with, the gutter, the tint behind added and removed rows,
//! the status line, and the comment accents -- is derived from that same syntax
//! theme's editor settings, so choosing a theme recolors the whole interface to
//! match its file coloring rather than pinning the chrome to one hand-tuned
//! palette.
//!
//! Colors taken straight from the theme (background, text, selection) are used
//! as-is. Colors the theme does not name are synthesized: structural text is
//! dimmed out of the foreground and nudged until it clears a legibility
//! threshold against its background, and the semantic accents (added green,
//! removed red, review gold, warning orange) keep a fixed hue so their meaning
//! stays constant across themes, with only their lightness adapted for
//! contrast.

use wiff_diff::{Rgb, ThemeChrome, theme_chrome};

/// The palette for rendering a diff: the syntect theme for content coloring
/// plus the backgrounds and accents the renderer applies around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// The syntect theme name used to color file content.
    pub syntax_theme: String,
    /// The background the whole diff view fills with, so a light or dark theme
    /// reads coherently regardless of the terminal's own background.
    pub background: Rgb,
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
    /// The background marking a search match within a row.
    pub search_match_bg: Rgb,
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
    /// The border color of a committed comment's box.
    pub comment_border_fg: Rgb,
    /// The color of a muted comment badge such as `resolved`.
    pub comment_flag_fg: Rgb,
    /// The color of a warning comment badge such as `shifted` or `outdated`.
    pub comment_warn_fg: Rgb,
    /// The color of the `draft` badge marking a comment with uncommitted edits.
    pub comment_draft_fg: Rgb,
}

/// The default palette for a dark terminal.
const FALLBACK_DARK_BG: Rgb = rgb(0x1e, 0x1e, 0x1e);

/// The fixed accent hues. Each keeps its meaning across every theme; only its
/// lightness is adapted so it stays legible on the theme's background. The
/// values are the base16-ocean accents, so the default dark theme reproduces
/// that palette exactly.
const GOLD: Rgb = rgb(0xeb, 0xcb, 0x8b);
const ORANGE: Rgb = rgb(0xd0, 0x87, 0x70);
const GREEN: Rgb = rgb(0xa3, 0xbe, 0x8c);
const BLUE: Rgb = rgb(0x8f, 0xa1, 0xb3);
const TEAL: Rgb = rgb(0x96, 0xb5, 0xb4);
const RED: Rgb = rgb(0xbf, 0x61, 0x6a);

/// The contrast ratio body text should clear against its background.
const TEXT_CONTRAST: f64 = 4.5;

/// The contrast ratio dimmed, secondary text should clear.
const DIM_CONTRAST: f64 = 3.0;

impl Theme {
    /// The palette for the built-in theme `name`, or `None` when no such syntax
    /// theme is bundled.
    pub fn named(name: &str) -> Option<Self> {
        Some(Self::from_chrome(name, theme_chrome(name)?))
    }

    /// The default palette for a dark terminal.
    pub fn dark() -> Self {
        Self::named(wiff_diff::highlight::DEFAULT_DARK_THEME)
            .expect("bundled base16-ocean.dark theme is always available")
    }

    /// The default palette for a light terminal.
    pub fn light() -> Self {
        Self::named(wiff_diff::highlight::DEFAULT_LIGHT_THEME)
            .expect("bundled InspiredGitHub theme is always available")
    }

    /// Derive the whole palette from syntax theme `name` and its `chrome`.
    ///
    /// The background and text come from the theme; the selection and search
    /// backgrounds too when it names them. The gutter and other structural text
    /// are dimmed out of the foreground toward the background and lifted back to
    /// a legible contrast. The added and removed tints blend a fixed green and
    /// red into the background, and the accents keep their hue with only their
    /// lightness adapted so each stays readable on the background.
    pub fn from_chrome(name: &str, chrome: ThemeChrome) -> Self {
        let bg = chrome.background.unwrap_or(FALLBACK_DARK_BG);
        let fg = chrome
            .foreground
            .unwrap_or_else(|| readable(bg, bg, TEXT_CONTRAST));
        let dim = mix(fg, bg, 0.45);
        let dimmer = mix(fg, bg, 0.6);
        Self {
            syntax_theme: name.to_string(),
            background: bg,
            gutter_fg: readable(dim, bg, DIM_CONTRAST),
            file_header_fg: readable(fg, bg, TEXT_CONTRAST),
            hunk_header_fg: readable(TEAL, bg, TEXT_CONTRAST),
            added_bg: mix(bg, GREEN, 0.18),
            removed_bg: mix(bg, RED, 0.18),
            added_emphasis_bg: mix(bg, GREEN, 0.4),
            removed_emphasis_bg: mix(bg, RED, 0.4),
            whitespace_bg: mix(bg, RED, 0.55),
            cursor_bg: chrome.selection.unwrap_or_else(|| mix(bg, fg, 0.22)),
            search_match_bg: chrome.find_highlight.unwrap_or_else(|| mix(bg, GOLD, 0.32)),
            fold_fg: readable(dimmer, bg, DIM_CONTRAST),
            status_fg: readable(fg, mix(bg, fg, 0.1), TEXT_CONTRAST),
            status_bg: mix(bg, fg, 0.1),
            review_fg: readable(GOLD, bg, TEXT_CONTRAST),
            comment_fg: readable(fg, bg, TEXT_CONTRAST),
            comment_author_fg: readable(BLUE, bg, TEXT_CONTRAST),
            comment_border_fg: readable(dimmer, bg, DIM_CONTRAST),
            comment_flag_fg: readable(dimmer, bg, DIM_CONTRAST),
            comment_warn_fg: readable(ORANGE, bg, TEXT_CONTRAST),
            comment_draft_fg: readable(GREEN, bg, TEXT_CONTRAST),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// A concise [`Rgb`] constructor for the accent table.
const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Linearly blend `a` toward `b` by `t` in `[0, 1]`, per channel.
fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let lerp = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    Rgb {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
    }
}

/// One sRGB channel's contribution to relative luminance, per the WCAG formula.
fn channel_luminance(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The WCAG relative luminance of a color, in `[0, 1]`.
fn luminance(c: Rgb) -> f64 {
    0.2126 * channel_luminance(c.r)
        + 0.7152 * channel_luminance(c.g)
        + 0.0722 * channel_luminance(c.b)
}

/// The WCAG contrast ratio between two colors, in `[1, 21]`.
fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Return `fg` unchanged when it already clears `min` contrast against `bg`,
/// otherwise blend it toward white or black -- whichever the background is
/// furthest from -- just far enough to reach the threshold, so a color stays as
/// close to its intended hue as legibility allows.
fn readable(fg: Rgb, bg: Rgb, min: f64) -> Rgb {
    if contrast(fg, bg) >= min {
        return fg;
    }
    let target = if luminance(bg) < 0.5 {
        rgb(0xff, 0xff, 0xff)
    } else {
        rgb(0x00, 0x00, 0x00)
    };
    let mut t = 0.0;
    while t < 1.0 {
        t += 0.05;
        let candidate = mix(fg, target, t);
        if contrast(candidate, bg) >= min {
            return candidate;
        }
    }
    target
}

/// Nudge `fg` so it stays at least as legible over `bg` as it is over
/// `reference`, never pushing past the body-text threshold. A diff tint or the
/// cursor wash the renderer lays over the theme's own background can only cut a
/// glyph's contrast; this lifts it back toward what the theme intended, so a
/// comment over an added-line tint, or any text under the cursor, stays as
/// readable as it was on the plain background. A glyph the theme already keeps
/// dim on purpose stays dim: the target never exceeds the contrast it had over
/// `reference`.
pub fn legible_over(fg: Rgb, bg: Rgb, reference: Rgb) -> Rgb {
    let target = contrast(fg, reference).min(TEXT_CONTRAST);
    readable(fg, bg, target)
}

#[cfg(test)]
mod tests {
    use super::{Rgb, Theme, legible_over, rgb};

    /// The whole palette rendered as `field #rrggbb` lines, so a test asserts
    /// every derived color a reviewer would see, not a single field.
    fn dump(theme: &Theme) -> String {
        let h = |c: wiff_diff::Rgb| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b);
        [
            format!("syntax_theme {}", theme.syntax_theme),
            format!("background {}", h(theme.background)),
            format!("gutter_fg {}", h(theme.gutter_fg)),
            format!("file_header_fg {}", h(theme.file_header_fg)),
            format!("hunk_header_fg {}", h(theme.hunk_header_fg)),
            format!("added_bg {}", h(theme.added_bg)),
            format!("removed_bg {}", h(theme.removed_bg)),
            format!("added_emphasis_bg {}", h(theme.added_emphasis_bg)),
            format!("removed_emphasis_bg {}", h(theme.removed_emphasis_bg)),
            format!("whitespace_bg {}", h(theme.whitespace_bg)),
            format!("cursor_bg {}", h(theme.cursor_bg)),
            format!("search_match_bg {}", h(theme.search_match_bg)),
            format!("fold_fg {}", h(theme.fold_fg)),
            format!("status_fg {}", h(theme.status_fg)),
            format!("status_bg {}", h(theme.status_bg)),
            format!("review_fg {}", h(theme.review_fg)),
            format!("comment_fg {}", h(theme.comment_fg)),
            format!("comment_author_fg {}", h(theme.comment_author_fg)),
            format!("comment_border_fg {}", h(theme.comment_border_fg)),
            format!("comment_flag_fg {}", h(theme.comment_flag_fg)),
            format!("comment_warn_fg {}", h(theme.comment_warn_fg)),
            format!("comment_draft_fg {}", h(theme.comment_draft_fg)),
        ]
        .join("\n")
    }

    #[test]
    fn the_dark_theme_takes_the_syntax_themes_own_colors_and_derives_the_rest() {
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&Theme::dark()),
            "syntax_theme base16-ocean.dark\n",
            "background #2b303b\n",
            "gutter_fg #7d828c\n",
            "file_header_fg #c0c5ce\n",
            "hunk_header_fg #96b5b4\n",
            "added_bg #414a4a\n",
            "removed_bg #463943\n",
            "added_emphasis_bg #5b695b\n",
            "removed_emphasis_bg #66444e\n",
            "whitespace_bg #7c4b55\n",
            "cursor_bg #4f5b66\n",
            "search_match_bg #686255\n",
            "fold_fg #767b84\n",
            "status_fg #c0c5ce\n",
            "status_bg #3a3f4a\n",
            "review_fg #ebcb8b\n",
            "comment_fg #c0c5ce\n",
            "comment_author_fg #8fa1b3\n",
            "comment_border_fg #767b84\n",
            "comment_flag_fg #767b84\n",
            "comment_warn_fg #d08770\n",
            "comment_draft_fg #a3be8c",
        );
    }

    #[test]
    fn the_light_theme_derives_a_legible_palette_over_a_light_background() {
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&Theme::light()),
            "syntax_theme InspiredGitHub\n",
            "background #ffffff\n",
            "gutter_fg #8e8e8e\n",
            "file_header_fg #323232\n",
            "hunk_header_fg #627675\n",
            "added_bg #eef3ea\n",
            "removed_bg #f3e3e4\n",
            "added_emphasis_bg #dae5d1\n",
            "removed_emphasis_bg #e5c0c3\n",
            "whitespace_bg #dca8ad\n",
            "cursor_bg #f8eec7\n",
            "search_match_bg #f8eec7\n",
            "fold_fg #939393\n",
            "status_fg #323232\n",
            "status_bg #ebebeb\n",
            "review_fg #81704c\n",
            "comment_fg #323232\n",
            "comment_author_fg #64717d\n",
            "comment_border_fg #939393\n",
            "comment_flag_fg #939393\n",
            "comment_warn_fg #9c6554\n",
            "comment_draft_fg #6a7c5b",
        );
    }

    #[test]
    fn an_unknown_theme_name_has_no_palette() {
        wince::assert_eq!(Theme::named("no such theme").is_none(), true);
    }

    /// `field #rrggbb` for one color, so a contrast test asserts the whole value.
    fn hex(c: Rgb) -> String {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    }

    #[test]
    fn legible_over_lifts_a_dim_glyph_a_tint_would_bury_and_leaves_a_clear_one() {
        // The base16-ocean comment gray reads on the theme background, but the
        // added-line tint sits between them and cuts its contrast, so it is
        // lifted back toward the contrast it had on the plain background. The
        // default text color already clears the tint, so it stays put.
        let background = rgb(0x2b, 0x30, 0x3b);
        let added_bg = rgb(0x41, 0x4a, 0x4a);
        let comment = rgb(0x65, 0x73, 0x7e);
        let text = rgb(0xc0, 0xc5, 0xce);
        let report = [
            format!("comment on background {}", hex(comment)),
            format!(
                "comment on tint      {}",
                hex(legible_over(comment, added_bg, background))
            ),
            format!("text on background    {}", hex(text)),
            format!(
                "text on tint          {}",
                hex(legible_over(text, added_bg, background))
            ),
        ]
        .join("\n");
        #[rustfmt::skip]
        wince::snapshot_str!(
            report,
            "comment on background #65737e\n",
            "comment on tint      #848f98\n",
            "text on background    #c0c5ce\n",
            "text on tint          #c0c5ce",
        );
    }
}
