//! Soft-wrapping a styled line to a column width.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Break `line` into visual lines no wider than `width` columns, breaking at a
/// space where one fits and hard-splitting a word longer than `width`. Each
/// output line keeps the original spans' styles. Column counts are in
/// characters, matching the rest of the renderer. A `width` of zero means no
/// wrapping is wanted, so `line` comes back as a single line unchanged.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let chars = flatten(line);
    if width == 0 || chars.len() <= width {
        return vec![line.clone()];
    }
    let mut lines = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    // The position just past the last space in `current`, the point a break
    // would land on so the space stays with the finished line and the next word
    // starts the following one.
    let mut last_break: Option<usize> = None;
    for &(ch, style) in &chars {
        current.push((ch, style));
        if ch == ' ' {
            last_break = Some(current.len());
        }
        if current.len() > width {
            let rest = match last_break {
                Some(at) if at > 0 => current.split_off(at),
                // No space to break on, so the word is wider than the line:
                // split it hard at the column limit and carry the remainder on.
                _ => current.split_off(width),
            };
            trim_trailing_spaces(&mut current);
            lines.push(regroup(&current));
            current = rest;
            last_break = current.iter().rposition(|&(c, _)| c == ' ').map(|p| p + 1);
        }
    }
    lines.push(regroup(&current));
    lines
}

/// Flatten the spans of `line` into a per-character list paired with the style
/// each character carries, so wrapping can break anywhere and regroup after.
fn flatten(line: &Line<'static>) -> Vec<(char, Style)> {
    let mut chars = Vec::new();
    for span in &line.spans {
        for ch in span.content.chars() {
            chars.push((ch, span.style));
        }
    }
    chars
}

/// Drop trailing spaces from a finished line, so a break after a run of spaces
/// does not leave them dangling against the right edge.
fn trim_trailing_spaces(chars: &mut Vec<(char, Style)>) {
    while chars.last().is_some_and(|&(c, _)| c == ' ') {
        chars.pop();
    }
}

/// Rebuild a [`Line`] from per-character styles, merging neighboring characters
/// that share a style back into one span.
fn regroup(chars: &[(char, Style)]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style: Option<Style> = None;
    for &(ch, ch_style) in chars {
        if style != Some(ch_style) && !text.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut text),
                style.unwrap_or_default(),
            ));
        }
        style = Some(ch_style);
        text.push(ch);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style.unwrap_or_default()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    use super::wrap_line;

    /// Render a wrapped result as one plain string per line, so a test reads the
    /// break points without span noise.
    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_line_within_the_width_is_returned_whole() {
        let line = Line::from("short enough");
        k9::assert_equal!(
            texts(&wrap_line(&line, 20)),
            vec!["short enough".to_string()]
        );
    }

    #[test]
    fn a_zero_width_leaves_the_line_unwrapped() {
        let line = Line::from("anything at all here");
        k9::assert_equal!(
            texts(&wrap_line(&line, 0)),
            vec!["anything at all here".to_string()]
        );
    }

    #[test]
    fn breaks_land_on_spaces_and_drop_the_broken_space() {
        // At width ten the run breaks after "the" and after "lazy", each break
        // consuming the space so no line begins or ends on one.
        let line = Line::from("the quick brown lazy fox");
        k9::assert_equal!(
            texts(&wrap_line(&line, 10)),
            vec![
                "the quick".to_string(),
                "brown lazy".to_string(),
                "fox".to_string(),
            ]
        );
    }

    #[test]
    fn a_word_wider_than_the_line_is_split_hard() {
        // "supercalifragilistic" has no space to break on, so it is cut at the
        // width and the remainder continues on the next line with the trailing
        // word.
        let line = Line::from("a supercalifragilistic word");
        k9::assert_equal!(
            texts(&wrap_line(&line, 8)),
            vec![
                "a".to_string(),
                "supercal".to_string(),
                "ifragili".to_string(),
                "stic".to_string(),
                "word".to_string(),
            ]
        );
    }

    #[test]
    fn each_wrapped_line_keeps_the_styles_of_its_spans() {
        // A line of two differently-styled words wraps between them, and each
        // fragment carries the color of the word it came from.
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("hello", red),
            Span::styled(" ", red),
            Span::styled("world", blue),
        ]);
        let wrapped = wrap_line(&line, 5);
        let rendered: Vec<Vec<(String, Style)>> = wrapped
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| (s.content.to_string(), s.style))
                    .collect()
            })
            .collect();
        k9::assert_equal!(
            rendered,
            vec![
                vec![("hello".to_string(), red)],
                vec![("world".to_string(), blue)],
            ]
        );
    }
}
