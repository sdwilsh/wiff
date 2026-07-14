//! The help overlay: a modal key reference the reviewer raises to remind
//! themselves what each key does.
//!
//! A reviewer coming to wiff has no menu bar to browse, so the bindings are
//! otherwise invisible. [`Help`] lists them grouped by purpose, reading the
//! active [`Keymap`] rather than a fixed table, so a reconfigured binding shows
//! as the key the reviewer actually presses. A reference taller than the space
//! it is given scrolls; selecting nothing, it exists only to be read and
//! dismissed.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use wiff_diff::Rgb;

use crate::action::ACTION_GROUPS;
use crate::keymap::Keymap;
use crate::render::color;

/// The colors the help overlay paints with, taken from the theme by the host.
#[derive(Debug, Clone, Copy)]
pub struct HelpColors {
    /// The border, title, and group-heading color.
    pub border: Rgb,
    /// The background color filling the modal.
    pub background: Rgb,
    /// The color of the key column.
    pub keys: Rgb,
    /// The color of a binding's description.
    pub text: Rgb,
    /// The color of the dismissal hint along the bottom.
    pub hint: Rgb,
}

/// The hint shown along the bottom of the overlay.
const HINT: &str = "any key to close";

/// One rendered row of the overlay.
enum Row {
    /// A group heading standing above its bindings.
    Heading(String),
    /// A binding: its keys and what it does.
    Binding { keys: String, description: String },
    /// A blank spacer between groups.
    Blank,
}

/// A modal key reference, scrolled with the reviewer's own navigation.
pub struct Help {
    rows: Vec<Row>,
    /// The width of the widest key label in characters.
    keys_width: usize,
    /// The width of the widest description in characters.
    desc_width: usize,
    /// The first row of the visible window into `rows`.
    top: usize,
    /// The number of rows the window shows, set by the host each frame from the
    /// space the modal is given.
    height: usize,
    colors: HelpColors,
}

/// The columns each binding is indented from the border.
const INDENT: usize = 2;

/// The columns between the key column and a description.
const GAP: usize = 2;

impl Help {
    /// Build the overlay from the bindings active in `keymap`, drawn with
    /// `colors`. Each action's keys are the chords bound to it, joined with a
    /// comma; an unbound action is dropped.
    pub fn new(keymap: &Keymap, colors: HelpColors) -> Self {
        let mut rows = Vec::new();
        let mut keys_width = 0;
        let mut desc_width = 0;
        for group in ACTION_GROUPS {
            let bound: Vec<(String, &'static str)> = group
                .actions
                .iter()
                .filter_map(|action| {
                    let keys = keymap
                        .chords(*action)
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    (!keys.is_empty()).then(|| (keys, action.description()))
                })
                .collect();
            if bound.is_empty() {
                continue;
            }
            if !rows.is_empty() {
                rows.push(Row::Blank);
            }
            rows.push(Row::Heading(group.name.to_string()));
            for (keys, description) in bound {
                keys_width = keys_width.max(keys.chars().count());
                desc_width = desc_width.max(description.chars().count());
                rows.push(Row::Binding {
                    keys,
                    description: description.to_string(),
                });
            }
        }
        Self {
            rows,
            keys_width,
            desc_width,
            top: 0,
            height: 0,
            colors,
        }
    }

    /// Scroll up one row, stopping at the first.
    pub fn scroll_up(&mut self) {
        self.top = self.top.saturating_sub(1);
    }

    /// Scroll down one row, stopping so the last row stays in view.
    pub fn scroll_down(&mut self) {
        self.top = (self.top + 1).min(self.max_top());
    }

    /// Scroll up by a window's worth of rows.
    pub fn page_up(&mut self) {
        self.top = self.top.saturating_sub(self.page());
    }

    /// Scroll down by a window's worth of rows.
    pub fn page_down(&mut self) {
        self.top = (self.top + self.page()).min(self.max_top());
    }

    /// Scroll to the first row.
    pub fn to_top(&mut self) {
        self.top = 0;
    }

    /// Scroll so the last row is in view.
    pub fn to_bottom(&mut self) {
        self.top = self.max_top();
    }

    /// Set how many rows the window shows, keeping the scroll within range.
    pub fn set_height(&mut self, height: usize) {
        self.height = height;
        self.top = self.top.min(self.max_top());
    }

    /// The number of reference rows, so the host can size the modal to them.
    pub fn list_len(&self) -> usize {
        self.rows.len()
    }

    /// The first visible row, for the host to place the scrollbar thumb.
    pub fn top(&self) -> usize {
        self.top
    }

    /// The modal's border and title color.
    pub fn border(&self) -> Rgb {
        self.colors.border
    }

    /// The background filling the modal, for the host to paint the border row
    /// and the cells the content does not cover.
    pub fn background(&self) -> Rgb {
        self.colors.background
    }

    /// The heading for the modal's border.
    pub fn title(&self) -> &str {
        "Key bindings"
    }

    /// The width of the modal's content inside its border: the indented key
    /// column, a gap, then the widest description, and never narrower than the
    /// hint.
    pub fn width(&self) -> usize {
        (INDENT + self.keys_width + GAP + self.desc_width).max(HINT.chars().count())
    }

    /// The modal's content lines for a content width of `width`: the visible
    /// window of rows, then a spacer and the dismissal hint. Rows are clipped to
    /// fit the width.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let background = color(self.colors.background);
        let end = (self.top + self.height).min(self.rows.len());
        let mut lines: Vec<Line<'static>> = (self.top..end)
            .map(|index| self.render_row(&self.rows[index], width, background))
            .collect();
        lines.push(Line::from(Span::styled(
            " ".repeat(width),
            Style::default().bg(background),
        )));
        lines.push(Line::from(Span::styled(
            format!("{:<width$}", clip(HINT, width)),
            Style::default().fg(color(self.colors.hint)).bg(background),
        )));
        lines
    }

    /// Render one row padded to `width`: a heading in the border color, a
    /// binding as its keys and description in their own colors, or a blank line.
    fn render_row(
        &self,
        row: &Row,
        width: usize,
        background: ratatui::style::Color,
    ) -> Line<'static> {
        match row {
            Row::Blank => Line::from(Span::styled(
                " ".repeat(width),
                Style::default().bg(background),
            )),
            Row::Heading(text) => Line::from(Span::styled(
                format!("{:<width$}", clip(text, width)),
                Style::default()
                    .fg(color(self.colors.border))
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            )),
            Row::Binding { keys, description } => {
                let keys = format!(
                    "{:indent$}{keys:<pad$}",
                    "",
                    indent = INDENT,
                    pad = self.keys_width
                );
                let keys = clip(&keys, width);
                let rest = width.saturating_sub(keys.chars().count());
                let gap = GAP.min(rest);
                let description = clip(description, rest - gap);
                let tail = rest - gap - description.chars().count();
                Line::from(vec![
                    Span::styled(
                        keys,
                        Style::default().fg(color(self.colors.keys)).bg(background),
                    ),
                    Span::styled(
                        format!("{}{description}{}", " ".repeat(gap), " ".repeat(tail)),
                        Style::default().fg(color(self.colors.text)).bg(background),
                    ),
                ])
            }
        }
    }

    /// A window's worth of rows for a page move, at least one.
    fn page(&self) -> usize {
        self.height.max(1)
    }

    /// The furthest the window can scroll while keeping the last row in view.
    fn max_top(&self) -> usize {
        self.rows.len().saturating_sub(self.height)
    }
}

/// Clip `text` to at most `width` columns.
fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::{Help, HelpColors};
    use crate::keymap::Keymap;
    use crate::render::testutil::dump;
    use wiff_diff::Rgb;

    /// The help colors used across the tests, distinct so each shows in a dump.
    fn colors() -> HelpColors {
        HelpColors {
            border: Rgb {
                r: 0x11,
                g: 0x11,
                b: 0x11,
            },
            background: Rgb {
                r: 0x22,
                g: 0x22,
                b: 0x22,
            },
            keys: Rgb {
                r: 0x33,
                g: 0x33,
                b: 0x33,
            },
            text: Rgb {
                r: 0x44,
                g: 0x44,
                b: 0x44,
            },
            hint: Rgb {
                r: 0x55,
                g: 0x55,
                b: 0x55,
            },
        }
    }

    #[test]
    fn the_overlay_opens_on_the_first_group_of_default_bindings() {
        // A short window shows the top of the reference: the first heading and
        // the bindings under it in the key and text colors, then the spacer and
        // the dismissal hint anchored below.
        let mut help = Help::new(&Keymap::defaults(), colors());
        help.set_height(5);
        let expected = "\
<#111111|#222222|b>Navigation                                                        
<#333333|#222222|->  down, j                <#444444|#222222|->  Move down one line                     
<#333333|#222222|->  up, k                  <#444444|#222222|->  Move up one line                       
<#333333|#222222|->  space, ctrl-f, pagedown<#444444|#222222|->  Scroll down one page                   
<#333333|#222222|->  b, ctrl-b, pageup      <#444444|#222222|->  Scroll up one page                     
<-|#222222|->                                                                  
<#555555|#222222|->any key to close                                                  
";
        wince::assert_eq!(dump(&help.lines(help.width())), expected.to_string());
    }

    #[test]
    fn scrolling_down_moves_the_window_and_stops_at_the_last_row() {
        // Paging to the bottom shows the final group with the last row in view
        // and the hint still anchored below.
        let mut help = Help::new(&Keymap::defaults(), colors());
        help.set_height(4);
        help.to_bottom();
        let expected = "\
<#333333|#222222|->  V                      <#444444|#222222|->  Compare against an earlier version     
<#333333|#222222|->  o                      <#444444|#222222|->  Open the file in your editor           
<#333333|#222222|->  h                      <#444444|#222222|->  Show this help                         
<#333333|#222222|->  q                      <#444444|#222222|->  Quit                                   
<-|#222222|->                                                                  
<#555555|#222222|->any key to close                                                  
";
        wince::assert_eq!(dump(&help.lines(help.width())), expected.to_string());
    }
}
